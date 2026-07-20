use kestrelc::{cache, codegen, lexer, parser, purity, wasm_codegen};

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
    let cache_mode = if wasm { "wasm" } else { "native" };
    let cache_key = cache::key(&src, cache_mode);
    let cache_ext = if wasm { "wasm" } else { "o" };
    if let Some(cached) = cache::read(&cache_key, cache_ext) {
        if wasm {
            let out_path = format!("{stem}.wasm");
            if let Err(e) = fs::write(&out_path, &cached) {
                eprintln!("kestrelc: failed to write '{out_path}': {e}");
                return ExitCode::FAILURE;
            }
            println!("kestrelc: wrote ./{out_path} (cached)");
            return ExitCode::SUCCESS;
        } else {
            return link_and_report(&cached, &stem, true);
        }
    }

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kestrelc: {} (line {})", e.message, e.line);
            return ExitCode::FAILURE;
        }
    };

    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kestrelc: {} (line {})", e.message, e.line);
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

    if !program.iter().any(|f| f.name == "main") {
        eprintln!("kestrelc: No 'main' function found");
        return ExitCode::FAILURE;
    }

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
        cache::write(&cache_key, cache_ext, &bytes);
        println!("kestrelc: wrote ./{out_path}");
        return ExitCode::SUCCESS;
    }

    let mut cg = match codegen::Codegen::new() {
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
    cache::write(&cache_key, cache_ext, &object_bytes);
    link_and_report(&object_bytes, &stem, false)
}

/// Writes `object_bytes` to `<stem>.o` and links it into `<stem>` with
/// the system `cc`. Shared by the normal compile path and the
/// cache-hit path (which skips straight here with a previously-cached
/// object file) — linking is cheap enough, and simple enough as "just
/// invoke the system linker," that it isn't itself worth caching
/// separately from the object file.
fn link_and_report(object_bytes: &[u8], stem: &str, from_cache: bool) -> ExitCode {
    let obj_path = format!("{stem}.o");
    let out_path = stem.to_string();

    if let Err(e) = fs::write(&obj_path, object_bytes) {
        eprintln!("kestrelc: failed to write '{obj_path}': {e}");
        return ExitCode::FAILURE;
    }

    let link_status = Command::new("cc").arg(&obj_path).arg("-o").arg(&out_path).status();

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
