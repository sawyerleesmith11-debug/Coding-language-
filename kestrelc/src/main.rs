use kestrelc::{cache, codegen, format_diagnostic, fusion, inline, lexer, parser, profile, purity, typecheck, wasm_codegen};

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (wasm, path) = match args.as_slice() {
        [_, flag, path] if flag == "--wasm" => (true, path.clone()),
        [_, path] => (false, path.clone()),
        _ => {
            eprintln!("usage: kestrelc [--wasm] <file.kes>");
            return ExitCode::FAILURE;
        }
    };
    let src = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kestrelc: can't read '{path}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let src_path = Path::new(&path);
    let stem = src_path.file_stem().unwrap().to_string_lossy();

    // A persistent, cross-invocation cache: if this exact source text
    // (for this exact backend) has compiled successfully before, skip
    // lexing/parsing/purity-checking/codegen entirely and reuse the
    // artifact. See kestrelc/src/cache.rs for the scope and the honest
    // gap between this and kestrel-DESIGN.md idea #1's full vision.
    //
    // The native backend's cache key additionally folds in a fingerprint
    // of the current runtime call-count profile (see profile.rs) — a
    // program that's actually been *run* since it last compiled may now
    // compile differently (see inline.rs), so a cache hit keyed only on
    // source text would silently keep serving the pre-profile object
    // forever, and the whole feedback loop would never actually fire.
    // `source_key` (profile-file naming) stays stable across that churn
    // on purpose; only the artifact key changes.
    let wasm_cache_key = cache::key(&src, "wasm");
    let source_key = cache::key(&src, "native");
    let profile_map = profile::read(&source_key);
    let profile_fingerprint = profile::fingerprint(&profile_map);
    let native_artifact_key = cache::artifact_key(&src, "native", &profile_fingerprint);
    if wasm {
        if let Some(cached) = cache::read(&wasm_cache_key, "wasm") {
            let out_path = format!("{stem}.wasm");
            if let Err(e) = fs::write(&out_path, &cached) {
                eprintln!("kestrelc: failed to write '{out_path}': {e}");
                return ExitCode::FAILURE;
            }
            println!("kestrelc: wrote ./{out_path} (cached)");
            return ExitCode::SUCCESS;
        }
    } else if let Some(cached) = cache::read(&native_artifact_key, "o") {
        return link_and_report(&cached, &stem, true);
    }

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kestrelc: {}", format_diagnostic(&src, &path, e.line, e.col, e.len, &e.message));
            return ExitCode::FAILURE;
        }
    };

    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kestrelc: {}", format_diagnostic(&src, &path, e.line, e.col, e.len, &e.message));
            return ExitCode::FAILURE;
        }
    };

    let purity_errors = purity::check_purity(&program);
    if !purity_errors.is_empty() {
        eprintln!("kestrelc: Purity check failed:");
        for e in &purity_errors {
            eprintln!("  {e}");
        }
        return ExitCode::FAILURE;
    }

    let pmap_errors = purity::check_parallel_map(&program);
    if !pmap_errors.is_empty() {
        eprintln!("kestrelc: parallel_map() check failed:");
        for e in &pmap_errors {
            eprintln!("  {e}");
        }
        return ExitCode::FAILURE;
    }

    let type_errors = typecheck::check_types(&program);
    if !type_errors.is_empty() {
        eprintln!("kestrelc: Type check failed:");
        for e in &type_errors {
            eprintln!("  {e}");
        }
        return ExitCode::FAILURE;
    }

    if !program.iter().any(|f| f.name == "main") {
        eprintln!("kestrelc: No 'main' function found");
        return ExitCode::FAILURE;
    }

    let program = fusion::fuse_loops(&program);

    if wasm {
        let bytes = match wasm_codegen::compile_to_wasm(&program) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("kestrelc: {}", e.0);
                return ExitCode::FAILURE;
            }
        };
        let out_path = format!("{stem}.wasm");
        if let Err(e) = fs::write(&out_path, &bytes) {
            eprintln!("kestrelc: failed to write '{out_path}': {e}");
            return ExitCode::FAILURE;
        }
        cache::write(&wasm_cache_key, "wasm", &bytes);
        println!("kestrelc: wrote ./{out_path}");
        return ExitCode::SUCCESS;
    }

    // Rewrites call sites of small, pure, previously-hot functions (per
    // `profile_map`, from the last time this exact source actually ran)
    // to inline their bodies directly — see inline.rs for the exact,
    // deliberately narrow eligibility rules. A no-op (returns `program`
    // unchanged) until a profile exists at all, which is exactly why the
    // very first compile of any given source behaves identically to
    // before this existed.
    let program = inline::inline_hot_fns(&program, &profile_map);

    let profile_path = profile::profile_path(&source_key).map(|p| p.to_string_lossy().into_owned());
    let mut cg = match codegen::Codegen::new(profile_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kestrelc: {}", e.0);
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = cg.compile_program(&program) {
        eprintln!("kestrelc: {}", e.0);
        return ExitCode::FAILURE;
    }
    let object_bytes = cg.finish();
    cache::write(&native_artifact_key, "o", &object_bytes);
    link_and_report(&object_bytes, &stem, false)
}

// Embedded at compile time so kestrelc is still a single self-contained
// binary — no separate runtime file to ship or lose track of. Written
// out fresh next to the object file on every native link (a handful of
// lines, negligible cost) rather than built once and cached, keeping
// `link_and_report` a single straightforward `cc` invocation.
const RUNTIME_C_SRC: &str = include_str!("../runtime/kestrelc_runtime.c");

/// Writes `object_bytes` to `<stem>.o` and links it, together with
/// kestrelc's small C runtime shim (real thread-parallel `parallel_map`
/// support — see `runtime/kestrelc_runtime.c`), into `<stem>` with the
/// system `cc`. Shared by the normal compile path and the cache-hit path
/// (which skips straight here with a previously-cached object file) —
/// linking is cheap enough, and simple enough as "just invoke the system
/// linker," that it isn't itself worth caching separately from the
/// object file. The runtime shim is linked in unconditionally, whether
/// or not the program actually uses parallel_map — it's a handful of
/// instructions otherwise, not worth a second linker pass to avoid.
fn link_and_report(object_bytes: &[u8], stem: &str, from_cache: bool) -> ExitCode {
    let obj_path = format!("{stem}.o");
    let out_path = stem.to_string();
    let runtime_path = "kestrelc_runtime.c";

    if let Err(e) = fs::write(&obj_path, object_bytes) {
        eprintln!("kestrelc: failed to write '{obj_path}': {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(runtime_path, RUNTIME_C_SRC) {
        eprintln!("kestrelc: failed to write '{runtime_path}': {e}");
        return ExitCode::FAILURE;
    }

    let link_status = Command::new("cc")
        .arg(&obj_path)
        .arg(runtime_path)
        .arg("-lpthread")
        .arg("-o")
        .arg(&out_path)
        .status();

    match link_status {
        Ok(status) if status.success() => {
            println!("kestrelc: wrote ./{out_path}{}", if from_cache { " (cached)" } else { "" });
            ExitCode::SUCCESS
        }
        Ok(status) => {
            eprintln!("kestrelc: link failed (cc exited with {status})");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("kestrelc: failed to invoke 'cc' linker: {e}");
            ExitCode::FAILURE
        }
    }
}
