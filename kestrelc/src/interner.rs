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
// every one of purity.rs/typecheck.rs/codegen.rs/wasm_codegen.rs/
// fusion.rs/inline.rs's public functions (and everything they call) —
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

#[derive(Default)]
struct Table {
    // `map` owns the only copy of each distinct string's bytes; `vec` is
    // a second, cheap (ptr+len+len) handle into the *same* allocation
    // via `Rc<str>`, so `resolve` can index by `Symbol` without a second
    // copy of the text.
    map: HashMap<Rc<str>, Symbol>,
    vec: Vec<Rc<str>>,
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

pub fn intern(s: &str) -> Symbol {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(&sym) = t.map.get(s) {
            return sym;
        }
        let rc: Rc<str> = Rc::from(s);
        let sym = Symbol(t.vec.len() as u32);
        t.vec.push(rc.clone());
        t.map.insert(rc, sym);
        sym
    })
}

pub fn resolve(sym: Symbol) -> Rc<str> {
    TABLE.with(|t| t.borrow().vec[sym.0 as usize].clone())
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
}
