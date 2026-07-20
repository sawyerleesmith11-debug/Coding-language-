// Persistent, cross-invocation compile cache for the `kestrelc` CLI —
// see kestrel-DESIGN.md idea #1 ("Persistent cross-run optimization
// cache"). This is a scoped-down first step toward that idea, not the
// full runtime-profile-guided pre-specialization it originally
// describes: it skips *redundant recompilation* of source that hasn't
// changed since it last compiled successfully (keyed by a hash of the
// source text), not runtime branch/shape profiling. Real, honest, and
// immediately useful for the common case of running `kestrelc` (or the
// editor's native engine) repeatedly on the same file during a dev
// loop — see cache::dir()'s doc comment for exactly where entries live.
//
// Only used by the native CLI (filesystem-backed); kestrelc-web has no
// filesystem, so the browser editor uses its own in-memory cache instead
// (see kestrel-editor.html's runNative()).

use std::fs;
use std::path::PathBuf;

/// Bumped whenever the cached artifact's format changes (e.g. codegen
/// output shape) — folded into the cache key so a version bump silently
/// invalidates every old entry instead of risking a stale/incompatible
/// hit. Cache misses are always safe (just slower); cache *hits* must
/// only ever happen for byte-identical input to a byte-identical
/// compiler, which this guards.
const CACHE_FORMAT_VERSION: &str = "v1";

/// Where cache entries live: `$KESTRELC_CACHE_DIR` if set, else
/// `$XDG_CACHE_HOME/kestrelc`, else `$HOME/.cache/kestrelc`, else `None`
/// (caching is silently skipped — a missing `$HOME` is rare but not
/// fatal to compiling).
pub fn dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("KESTRELC_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(d).join("kestrelc"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home).join(".cache").join("kestrelc"));
    }
    None
}

/// FNV-1a, 64-bit. Not cryptographic — doesn't need to be, this is a
/// cache key, not a security boundary — but deterministic forever
/// (unlike e.g. `std::collections::hash_map::DefaultHasher`, whose
/// algorithm Rust explicitly reserves the right to change between
/// releases), so cache entries stay valid across `kestrelc` rebuilds
/// with the same `CACHE_FORMAT_VERSION`.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The cache key for a given source text + compilation mode ("native" or
/// "wasm" — the two backends produce different artifacts from the same
/// source, so they need distinct entries).
pub fn key(src: &str, mode: &str) -> String {
    let mut input = String::with_capacity(src.len() + 16);
    input.push_str(CACHE_FORMAT_VERSION);
    input.push('|');
    input.push_str(mode);
    input.push('|');
    input.push_str(src);
    format!("{:016x}", fnv1a64(input.as_bytes()))
}

/// The full path a given key's cache entry would live at, if caching is
/// available at all (see `dir()`).
pub fn path(key: &str, ext: &str) -> Option<PathBuf> {
    dir().map(|d| d.join(format!("{key}.{ext}")))
}

/// Reads a cache entry if present. Any I/O error (missing dir, missing
/// file, permissions) is just treated as a cache miss — caching is
/// always an optimization, never something compilation should fail over.
pub fn read(key: &str, ext: &str) -> Option<Vec<u8>> {
    let p = path(key, ext)?;
    fs::read(p).ok()
}

/// Writes a cache entry. Best-effort: a failure here (read-only
/// filesystem, disk full, whatever) is silently ignored — the compile
/// already succeeded and its output was already written to the real
/// output path, so a cache-write failure must never turn into a
/// user-visible compile failure.
pub fn write(key: &str, ext: &str, bytes: &[u8]) {
    let Some(p) = path(key, ext) else { return };
    if let Some(parent) = p.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = fs::write(p, bytes);
}
