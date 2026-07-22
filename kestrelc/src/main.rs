use kestrelc::error::{KestrelcError, KestrelcWarning};
use kestrelc::{cache, codegen, cse, format_diagnostic, fusion, inline, lexer, parser, profile, purity, resolve, typecheck};

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

/// Prints one error, `format_diagnostic`'s full `file:line:col:` + caret
/// treatment when it has a real position, or just the bare message when
/// it doesn't (see `KestrelcError::internal` — a zero span means "an
/// internal compiler-level failure, not something in your source").
fn report_one(src: &str, path: &str, e: &KestrelcError) {
    if e.span.line == 0 {
        eprintln!("kestrelc: {}", e.message);
    } else {
        eprintln!("kestrelc: {}", format_diagnostic(src, path, e.span.line, e.span.col, e.span.len.max(1), &e.message));
    }
}

/// Prints a header line (from `e.kind`, shared across every error in
/// `errors` — a checker only ever reports its own kind) followed by
/// every error, indented, through `report_one`'s same formatting.
fn report_many(src: &str, path: &str, errors: &[KestrelcError]) {
    if let Some(first) = errors.first() {
        eprintln!("kestrelc: {}:", first.kind.label());
    }
    for e in errors {
        if e.span.line == 0 {
            eprintln!("  {}", e.message);
        } else {
            eprintln!("  {}", format_diagnostic(src, path, e.span.line, e.span.col, e.span.len.max(1), &e.message));
        }
    }
}

/// Prints every warning, same file:line:col + caret formatting as a
/// real error, prefixed "warning" instead of "kestrelc:" so it reads as
/// distinct from a build-failing message. Never affects the exit code —
/// unlike `report_one`/`report_many`, callers don't return
/// `ExitCode::FAILURE` after this.
fn report_warnings(src: &str, path: &str, warnings: &[KestrelcWarning]) {
    for w in warnings {
        if w.span.line == 0 {
            eprintln!("kestrelc: warning: {}", w.message);
        } else {
            eprintln!("kestrelc: warning: {}", format_diagnostic(src, path, w.span.line, w.span.col, w.span.len.max(1), &w.message));
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let [_, cmd, path] = args.as_slice() {
        if cmd == "watch" {
            return kestrelc::watch::run(path);
        }
    }
    if let [_, cmd] = args.as_slice() {
        if cmd == "watch" {
            eprintln!("usage: kestrelc watch <file.kes>");
            return ExitCode::FAILURE;
        }
    }
    let path = match args.as_slice() {
        [_, path] => path.clone(),
        _ => {
            eprintln!("usage: kestrelc <file.kes>\n       kestrelc watch <file.kes>");
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
    let source_key = cache::key(&src, "native");
    let profile_map = profile::read(&source_key);
    let profile_fingerprint = profile::fingerprint(&profile_map);
    let native_artifact_key = cache::artifact_key(&src, "native", &profile_fingerprint);
    if let Some(cached_bin) = cache::read(&native_artifact_key, "bin") {
        // The fast path: not just the object file but the *linked*
        // binary is cached, so this skips `cc` entirely -- on this
        // system that's ~1s of fixed linker-invocation overhead that
        // was previously paid on every single cached compile, even
        // though nothing about the output could have changed. Same
        // artifact key as the object-file cache below, so a hit here
        // only ever happens for byte-identical source + profile state.
        return write_and_report_binary(&cached_bin, &stem, true);
    } else if let Some(cached) = cache::read(&native_artifact_key, "o") {
        return link_and_report(&cached, &stem, true, &native_artifact_key);
    }

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(e) => {
            report_one(&src, &path, &e);
            return ExitCode::FAILURE;
        }
    };

    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            report_one(&src, &path, &e);
            return ExitCode::FAILURE;
        }
    };

    let fns = resolve::build_fn_table(&program);
    let structs = resolve::build_struct_table(&program);

    let resolve_errors = resolve::resolve(&program, &fns, &structs);
    if !resolve_errors.is_empty() {
        report_many(&src, &path, &resolve_errors);
        return ExitCode::FAILURE;
    }
    report_warnings(&src, &path, &resolve::check_size_warnings(&program));

    let purity_errors = purity::check_purity(&program, &fns);
    if !purity_errors.is_empty() {
        report_many(&src, &path, &purity_errors);
        return ExitCode::FAILURE;
    }

    let pmap_errors = purity::check_parallel_map(&program, &fns);
    if !pmap_errors.is_empty() {
        report_many(&src, &path, &pmap_errors);
        return ExitCode::FAILURE;
    }

    let type_errors = typecheck::check_types(&program, &fns);
    if !type_errors.is_empty() {
        report_many(&src, &path, &type_errors);
        return ExitCode::FAILURE;
    }

    if !program.fns.iter().any(|f| f.name == kestrelc::interner::well_known::main()) {
        eprintln!("kestrelc: No 'main' function found");
        return ExitCode::FAILURE;
    }

    let program = fusion::fuse_loops(&program);
    let program = cse::eliminate_common_calls(&program);

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
            report_one(&src, &path, &e);
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = cg.compile_program(&program) {
        report_one(&src, &path, &e);
        return ExitCode::FAILURE;
    }
    let object_bytes = cg.finish();
    cache::write(&native_artifact_key, "o", &object_bytes);
    link_and_report(&object_bytes, &stem, false, &native_artifact_key)
}

/// Writes previously-linked binary bytes (from the `"bin"` cache) straight
/// to `<stem>`, skipping `cc` entirely -- the fast path for a genuine
/// cache hit. On Unix the executable bit isn't preserved by a plain
/// `fs::write`, so it's set explicitly; on Windows there's no equivalent
/// permission bit to worry about.
fn write_and_report_binary(bin_bytes: &[u8], stem: &str, from_cache: bool) -> ExitCode {
    let out_path = stem.to_string();
    // Match cc's own naming exactly (see link_and_report's comment on
    // actual_out_path) so the restored file is findable the same way a
    // freshly linked one is -- both by a shell typing "./stem" and by
    // Command::new("./stem") (Windows resolves the .exe automatically
    // for process launch either way; this is about where the bytes
    // actually land on disk).
    let actual_out_path = if cfg!(windows) { format!("{out_path}.exe") } else { out_path.clone() };
    if let Err(e) = fs::write(&actual_out_path, bin_bytes) {
        eprintln!("kestrelc: failed to write '{actual_out_path}': {e}");
        return ExitCode::FAILURE;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&actual_out_path, fs::Permissions::from_mode(0o755)) {
            eprintln!("kestrelc: failed to set '{actual_out_path}' executable: {e}");
            return ExitCode::FAILURE;
        }
    }
    println!("kestrelc: wrote ./{out_path}{}", if from_cache { " (cached)" } else { "" });
    ExitCode::SUCCESS
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
/// system `cc`. Shared by the normal compile path and the object-cache-hit
/// path (which skips straight here with a previously-cached object file
/// but no cached *linked binary* yet — see `write_and_report_binary` for
/// the faster path once one exists). The runtime shim is linked in
/// unconditionally, whether or not the program actually uses
/// parallel_map — it's a handful of instructions otherwise, not worth a
/// second linker pass to avoid.
///
/// On a successful link, also caches the resulting binary under
/// `artifact_key` (the `"bin"` extension) — `cc` invocation turned out to
/// be a fixed ~1s cost on Windows even for a trivial program, paid again
/// on every cache "hit" that only skipped codegen, not linking. Caching
/// the linked bytes lets a later invocation with the same artifact key
/// skip straight to `write_and_report_binary`, avoiding `cc` entirely.
fn link_and_report(object_bytes: &[u8], stem: &str, from_cache: bool, artifact_key: &str) -> ExitCode {
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
            // mingw's cc always appends .exe to its -o target on Windows,
            // regardless of the extension-less name it was given -- the
            // shell (and Windows' own CreateProcess) transparently
            // resolves "./stem" to "./stem.exe" when *running* a
            // program, but a literal fs::read of the bare name finds
            // nothing, since that resolution is a process-launch
            // convenience, not a filesystem one.
            let actual_out_path = if cfg!(windows) { format!("{out_path}.exe") } else { out_path.clone() };
            if let Ok(bin_bytes) = fs::read(&actual_out_path) {
                cache::write(artifact_key, "bin", &bin_bytes);
            }
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
