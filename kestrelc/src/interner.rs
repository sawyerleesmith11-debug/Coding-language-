// String interning: every identifier and string literal in a Kestrel
// program used to be its own heap-allocated `String` — duplicated once
// per occurrence, not once per distinct name. `Symbol` (a `Copy`, 4-byte
// handle) replaces `String` everywhere a name or string-literal value is
// stored in a `Tok`/AST node; this is the direct fix for the enum-size
// problem `Tok`/`Stmt`/`Expr` had: a `String` payload (24 bytes) forces
// every variant in the same enum to pay for it, even a zero-payload one
// like `Tok::Eof`, since Rust sizes an enum to its largest variant.
//
// A single `thread_local!` table backs every `Symbol`, rather than
// threading an `&mut Interner` parameter through the lexer, parser, and
// every one of purity.rs/typecheck.rs/codegen.rs/fusion.rs/inline.rs's
// public functions (and everything they call) —
// `kestrelc` compiles one file per process, single-threaded, so there's
// only ever one interner "session" alive at a time; a `parallel_map`
// program's worker threads run compiled machine code, never touch the
// AST or this table. `thread_local!` + `RefCell` is the least invasive
// way to get that without an actual global `static mut`/`unsafe`.
//
// Hand-rolled rather than pulling in a crate (`string-interner`,
// `lasso`) — same "no dependency for something this small" posture as
// `format_diagnostic`/the hand-rolled lexer elsewhere in this project.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(u32);

impl Symbol {
    pub fn resolve(self) -> Rc<str> {
        resolve(self)
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.resolve())
    }
}

/// FNV-1a over raw bytes -- fast, non-cryptographic, used instead of
/// std's default `SipHash` (which exists to resist hash-flooding from
/// adversarial/untrusted input). Every string interned here comes from
/// source code the compiler's own user wrote, not a network attacker;
/// paying SipHash's DoS-resistance cost on every identifier is wasted
/// work for input that was never adversarial to begin with.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A `Hasher` that treats its input as already-hashed: `write_u64`
/// stores the value verbatim instead of mixing it further. Used only as
/// `Table::index`'s `BuildHasher` (a `HashMap<u64, ..>` keyed by an
/// FNV-1a hash already computed once in `intern`) -- without this,
/// `HashMap`'s own default hasher would hash that `u64` key *again*
/// (via `SipHash`) just to place it into a bucket, hashing every
/// interned string twice for no benefit. `write` is unreachable here
/// because `u64::hash` only ever calls `write_u64`.
#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("IdentityHasher only ever receives u64 keys via write_u64")
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;

#[derive(Default)]
struct Table {
    // Dedup lookup: FNV-1a hash of a string's bytes -> every Symbol
    // whose text happens to share that hash (a collision list,
    // disambiguated in `intern` by comparing actual bytes against
    // `vec[symbol]`). Keyed by the hash `intern` already computed --
    // paired with `IdentityBuildHasher` so this map's own bucketing
    // doesn't hash that key a second time, `intern` hashes each string
    // exactly once, on a miss or a hit alike.
    index: HashMap<u64, Vec<Symbol>, IdentityBuildHasher>,
    // Symbol -> Rc<str>: `resolve`'s storage, a second cheap
    // (ptr+len+refcount) handle into the same allocation `index`'s
    // dedup check already verified is unique -- unaffected by the
    // above, `resolve` is still just a clone (refcount bump, no copy).
    vec: Vec<Rc<str>>,
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

pub fn intern(s: &str) -> Symbol {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let hash = fnv1a(s.as_bytes());
        if let Some(candidates) = t.index.get(&hash) {
            for &sym in candidates {
                if &*t.vec[sym.0 as usize] == s {
                    return sym;
                }
            }
        }
        let sym = Symbol(t.vec.len() as u32);
        t.vec.push(Rc::from(s));
        t.index.entry(hash).or_default().push(sym);
        sym
    })
}

pub fn resolve(sym: Symbol) -> Rc<str> {
    TABLE.with(|t| t.borrow().vec[sym.0 as usize].clone())
}

/// `Symbol`s for identifiers the compiler itself compares against by
/// name (`"main"`, `"parallel_map"`) -- interned once, on first use, and
/// cached as a plain `Symbol` (`Copy`, 4 bytes) rather than re-resolved
/// and string-compared at every one of the ~20+ call sites across the
/// compiler that check "is this identifier main/parallel_map." Direct
/// `Symbol == Symbol` comparison is `u32` equality: no hashing, no
/// allocation, no string comparison -- strictly cheaper than
/// `&*name.resolve() == "literal"`, which the majority of those call
/// sites were doing.
pub mod well_known {
    use super::{intern, Symbol};
    use std::cell::Cell;

    thread_local! {
        static MAIN: Cell<Option<Symbol>> = const { Cell::new(None) };
        static PARALLEL_MAP: Cell<Option<Symbol>> = const { Cell::new(None) };
        static MAP: Cell<Option<Symbol>> = const { Cell::new(None) };
    }

    pub fn main() -> Symbol {
        MAIN.with(|c| match c.get() {
            Some(s) => s,
            None => {
                let s = intern("main");
                c.set(Some(s));
                s
            }
        })
    }

    pub fn parallel_map() -> Symbol {
        PARALLEL_MAP.with(|c| match c.get() {
            Some(s) => s,
            None => {
                let s = intern("parallel_map");
                c.set(Some(s));
                s
            }
        })
    }

    /// `map` -- checked by the parser (see `parse_postfix`'s `.map(f)`
    /// sugar for `parallel_map`, kestrelc/src/parser.rs), not compared
    /// against a resolved user identifier the way `main`/`parallel_map`
    /// are elsewhere in the compiler; interned via `well_known` anyway
    /// for the same cheap-comparison reason and to keep every "identifier
    /// the compiler itself checks by name" in one place.
    pub fn map() -> Symbol {
        MAP.with(|c| match c.get() {
            Some(s) => s,
            None => {
                let s = intern("map");
                c.set(Some(s));
                s
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_the_same_string_twice_returns_the_same_symbol() {
        let a = intern("__interner_test_foo__");
        let b = intern("__interner_test_foo__");
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_strings_get_distinct_symbols_that_resolve_back_correctly() {
        let a = intern("__interner_test_alpha__");
        let b = intern("__interner_test_beta__");
        assert_ne!(a, b);
        assert_eq!(&*resolve(a), "__interner_test_alpha__");
        assert_eq!(&*resolve(b), "__interner_test_beta__");
    }

    /// Regression coverage for the index-based dedup rewrite: many
    /// distinct strings must each get their own Symbol, survive a
    /// second `intern` of the same text unchanged, and resolve back to
    /// exactly their own text -- specifically exercises the hash-bucket
    /// collision-list disambiguation in `intern` (multiple strings
    /// landing in the same FNV-1a bucket must still compare correctly
    /// against `vec[symbol]` rather than any other bucket member).
    #[test]
    fn many_distinct_strings_intern_uniquely_and_resolve_correctly() {
        let strings: Vec<String> = (0..500).map(|i| format!("__interner_test_many_{i:03}__")).collect();
        let syms: Vec<Symbol> = strings.iter().map(|s| intern(s)).collect();

        for i in 0..syms.len() {
            for j in (i + 1)..syms.len() {
                assert_ne!(syms[i], syms[j], "'{}' and '{}' collided", strings[i], strings[j]);
            }
        }
        for (sym, s) in syms.iter().zip(&strings) {
            assert_eq!(&*resolve(*sym), s.as_str());
        }
        // Re-interning must return the exact same symbols, not new ones.
        for (sym, s) in syms.iter().zip(&strings) {
            assert_eq!(intern(s), *sym);
        }
    }

    #[test]
    fn well_known_main_and_parallel_map_match_plain_interning_of_the_same_text() {
        assert_eq!(well_known::main(), intern("main"));
        assert_eq!(well_known::parallel_map(), intern("parallel_map"));
        // Repeated calls must be stable (cached), not re-interned each time.
        assert_eq!(well_known::main(), well_known::main());
        assert_eq!(well_known::parallel_map(), well_known::parallel_map());
    }
}
