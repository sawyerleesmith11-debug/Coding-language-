// The runtime half of kestrel-DESIGN.md idea #1's "full runtime-profile-
// guided compile cache": a compiled native binary counts how many times
// each of its own functions actually ran, and writes those counts to a
// small text file next to its compile-cache entry when the process exits
// (see codegen.rs's profile instrumentation — every function increments
// its own counter on entry, and `main`'s epilogue flushes all of them via
// `kestrelc_profile_record` in runtime/kestrelc_runtime.c). The *next*
// `kestrelc` invocation on unchanged source reads this file back and uses
// it to decide which small pure functions are worth inlining (see
// inline.rs) — a real runtime feedback loop, not just "skip
// recompilation" (that's cache.rs). Scope, honestly: call counts only,
// not the branch/shape profiling kestrel-DESIGN.md's idea #1 originally
// describes.
//
// Keyed by `cache::key(src, "native")` — stable across recompiles of the
// same source regardless of what profile data currently exists, so the
// profile file's *path* never moves even as its *contents* evolve run
// over run. The compiled *artifact* cache (main.rs) uses a different key
// that folds in a fingerprint of this file's contents, so a stale
// (pre-profile or differently-profiled) cached object is never reused
// once fresher profile data exists — see cache::artifact_key.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Where a given source's profile data lives, if caching is available at
/// all (see cache::dir()). `source_key` should be `cache::key(src,
/// "native")` — the same stable key used to decide whether the compile
/// artifact cache even applies, not the fingerprint-folded artifact key.
pub fn profile_path(source_key: &str) -> Option<PathBuf> {
    crate::cache::dir().map(|d| d.join(format!("{source_key}.profile")))
}

/// Reads a previous run's call counts, if any exist yet. Any I/O error or
/// malformed line is just treated as "no data for that entry" — like
/// cache.rs, this is always an optimization, never something that should
/// make compilation fail or behave differently on a corrupt file.
pub fn read(source_key: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    let Some(p) = profile_path(source_key) else { return map };
    let Ok(text) = fs::read_to_string(p) else { return map };
    for line in text.lines() {
        let Some(space_idx) = line.rfind(' ') else { continue };
        let (name, count_s) = line.split_at(space_idx);
        let count_s = count_s.trim_start();
        if let Ok(count) = count_s.parse::<u64>() {
            map.insert(name.to_string(), count);
        }
    }
    map
}

/// A deterministic fingerprint of a profile snapshot (empty string for
/// "no profile yet"), used to fold into the compile-artifact cache key —
/// see cache::artifact_key. Sorted by function name first so it doesn't
/// depend on HashMap iteration order, which is randomized per-process.
pub fn fingerprint(profile: &HashMap<String, u64>) -> String {
    if profile.is_empty() {
        return String::new();
    }
    let mut entries: Vec<(&String, &u64)> = profile.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut buf = String::new();
    for (name, count) in entries {
        buf.push_str(name);
        buf.push('=');
        buf.push_str(&count.to_string());
        buf.push(';');
    }
    format!("{:016x}", crate::cache::fnv1a64(buf.as_bytes()))
}
