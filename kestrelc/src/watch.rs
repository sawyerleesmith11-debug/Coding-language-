use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Blocks until either the channel disconnects (returns `false`) or at
/// least one event has arrived and then no further event arrives for
/// `timeout` (returns `true`) -- coalesces a burst of rapid-fire events
/// from one logical file save into a single "go" signal.
pub(crate) fn drain_debounced(rx: &Receiver<()>, timeout: Duration) -> bool {
    if rx.recv().is_err() {
        return false;
    }
    loop {
        match rx.recv_timeout(timeout) {
            Ok(()) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return true,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

use crate::error::KestrelcError;
use crate::{format_diagnostic, jit_codegen, lexer, parser, purity, resolve, typecheck};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::mpsc::channel;

/// `kestrelc watch <file.kes>` -- on every save, recompile and rerun.
///
/// Shells out to the current `kestrelc` executable rather than calling
/// the compiler pipeline in-process: this is the exact same code path
/// `kestrelc <file.kes>` already uses and already has tests for, so
/// watch mode can't drift from normal compile behavior.
pub fn run(path: &str) -> ExitCode {
    let src_path = Path::new(path);
    if !src_path.exists() {
        eprintln!("kestrelc: can't read '{path}': No such file or directory");
        return ExitCode::FAILURE;
    }
    let stem = match src_path.file_stem() {
        Some(s) => s.to_string_lossy().into_owned(),
        None => {
            eprintln!("kestrelc: '{path}' has no file stem");
            return ExitCode::FAILURE;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("kestrelc: can't find my own executable path: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (tx, rx) = channel::<()>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        match res {
            Ok(event) => {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    let _ = tx.send(());
                }
            }
            // A dev-loop tool's whole point is fast feedback -- a
            // watcher that silently goes deaf (OS-level watch failure,
            // e.g. the watched file gets deleted rather than
            // edited-and-rewritten) is worse than a crash, since the
            // process looks alive with no indication it stopped doing
            // anything. Surface it and keep going; the next real event
            // (if the watch recovers) still works.
            Err(e) => eprintln!("kestrelc: watch error: {e}"),
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("kestrelc: failed to start file watcher: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = watcher.watch(src_path, RecursiveMode::NonRecursive) {
        eprintln!("kestrelc: failed to watch '{path}': {e}");
        return ExitCode::FAILURE;
    }

    println!("kestrelc: watching {path} (Ctrl+C to stop)");
    compile_and_run(&exe, path, &stem);

    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);
    while drain_debounced(&rx, DEBOUNCE) {
        compile_and_run(&exe, path, &stem);
    }
    ExitCode::SUCCESS
}

fn compile_and_run(exe: &Path, path: &str, stem: &str) {
    let started = std::time::Instant::now();
    print!("\x1B[2J\x1B[1;1H"); // clear screen, move cursor to top-left
    println!("kestrelc watch: {path}");

    match try_jit(path) {
        JitOutcome::Ran(result) => {
            println!("--- exited with code {result} (finished in {:.2?}, in-process) ---", started.elapsed());
            return;
        }
        JitOutcome::CompileError => {
            println!("--- failed in {:.2?} ---", started.elapsed());
            return;
        }
        JitOutcome::Unsupported(reason) => {
            // Not a failure -- this program uses a construct JIT mode
            // doesn't support yet (arrays, structs, parallel_map -- see
            // jit_codegen.rs's module doc comment). Fall through to the
            // normal self-invoke/AOT path below, which handles
            // everything. Printed so the user knows why this save felt
            // like the old, slower path instead of silently varying.
            println!("kestrelc watch: {reason} -- using the normal compile path for this run");
        }
    }

    let compile_status = Command::new(exe).arg(path).status();
    match compile_status {
        Ok(status) if status.success() => {}
        Ok(_) => {
            println!("--- failed in {:.2?} ---", started.elapsed());
            return; // compiler already printed its own error
        }
        Err(e) => {
            eprintln!("kestrelc: failed to invoke self ('{}'): {e}", exe.display());
            return;
        }
    }

    // Matches link_and_report's own output naming (kestrelc/src/main.rs)
    // exactly: `-o <stem>`, no extension appended, same as this
    // project's own integration tests already invoke the compiled
    // binary by.
    let bin_path = format!("./{stem}");
    println!("--- running {bin_path} ---");
    match Command::new(&bin_path).status() {
        Ok(status) => println!("--- exited with {status} (finished in {:.2?}) ---", started.elapsed()),
        Err(e) => eprintln!("kestrelc: failed to run '{bin_path}': {e}"),
    }
}

enum JitOutcome {
    /// Compiled and ran successfully in-process; carries `main`'s
    /// returned i64.
    Ran(i64),
    /// A real compile error (lex/parse/resolve/purity/typecheck, or a
    /// JIT-codegen-internal failure) -- already printed to stderr by the
    /// time this is returned. Not a reason to fall back to the AOT path,
    /// since that path would hit the exact same front-end error.
    CompileError,
    /// This program uses a construct JIT mode doesn't support yet (see
    /// jit_codegen::check_jit_supported) -- fall back to the AOT path,
    /// not a real failure.
    Unsupported(String),
}

/// Runs the full front end (lex/parse/resolve/purity/typecheck -- the
/// same pipeline main.rs's own compile path uses, reused directly rather
/// than duplicated) and, if the program is within JIT mode's supported
/// subset, JIT-compiles and runs it immediately in-process. See
/// jit_codegen.rs's module doc comment for exactly what's supported.
fn try_jit(path: &str) -> JitOutcome {
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kestrelc: can't read '{path}': {e}");
            return JitOutcome::CompileError;
        }
    };

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(e) => {
            report_error(&src, path, &e);
            return JitOutcome::CompileError;
        }
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            report_error(&src, path, &e);
            return JitOutcome::CompileError;
        }
    };

    let fns = resolve::build_fn_table(&program);
    let structs = resolve::build_struct_table(&program);
    let resolve_errors = resolve::resolve(&program, &fns, &structs);
    if !resolve_errors.is_empty() {
        report_errors(&src, path, &resolve_errors);
        return JitOutcome::CompileError;
    }
    let purity_errors = purity::check_purity(&program, &fns);
    if !purity_errors.is_empty() {
        report_errors(&src, path, &purity_errors);
        return JitOutcome::CompileError;
    }
    let pmap_errors = purity::check_parallel_map(&program, &fns);
    if !pmap_errors.is_empty() {
        report_errors(&src, path, &pmap_errors);
        return JitOutcome::CompileError;
    }
    let type_errors = typecheck::check_types(&program, &fns);
    if !type_errors.is_empty() {
        report_errors(&src, path, &type_errors);
        return JitOutcome::CompileError;
    }
    if !program.fns.iter().any(|f| &*f.name.resolve() == "main") {
        eprintln!("kestrelc: No 'main' function found");
        return JitOutcome::CompileError;
    }

    if let Err(e) = jit_codegen::check_jit_supported(&program) {
        return JitOutcome::Unsupported(e.message);
    }

    let mut cg = match jit_codegen::JitCodegen::new() {
        Ok(c) => c,
        Err(e) => {
            report_error(&src, path, &e);
            return JitOutcome::CompileError;
        }
    };
    if let Err(e) = cg.compile_program(&program) {
        report_error(&src, path, &e);
        return JitOutcome::CompileError;
    }
    match cg.finish_and_run() {
        Ok(result) => JitOutcome::Ran(result),
        Err(e) => {
            report_error(&src, path, &e);
            JitOutcome::CompileError
        }
    }
}

/// Matches main.rs's own report_one exactly (duplicated, not shared --
/// main.rs is a separate bin crate, not something this lib module can
/// call into).
fn report_error(src: &str, path: &str, e: &KestrelcError) {
    if e.span.line == 0 {
        eprintln!("kestrelc: {}", e.message);
    } else {
        eprintln!("kestrelc: {}", format_diagnostic(src, path, e.span.line, e.span.col, e.span.len.max(1), &e.message));
    }
}

/// Matches main.rs's own report_many exactly, same reason as report_error.
fn report_errors(src: &str, path: &str, errors: &[KestrelcError]) {
    if let Some(first) = errors.first() {
        eprintln!("kestrelc: {}:", first.kind.label());
    }
    for e in errors {
        report_error(src, path, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::thread;

    #[test]
    fn a_burst_of_events_coalesces_into_one_debounced_signal() {
        let (tx, rx) = channel::<()>();
        // Keep a clone alive past the burst so the channel doesn't
        // disconnect right around the same time the debounce window
        // closes -- that race (sender dropping at ~25ms vs. a 50ms
        // debounce timeout) would make drain_debounced legitimately
        // observe Disconnected instead of Timeout, which isn't what
        // this assertion means to exercise.
        let tx_keepalive = tx.clone();
        thread::spawn(move || {
            for _ in 0..5 {
                tx.send(()).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
            // this clone drops here; tx_keepalive still holds the channel open
        });
        // First call should return true once the burst goes quiet.
        assert!(drain_debounced(&rx, Duration::from_millis(50)));
        // Now disconnect for real, well after the debounce window above.
        drop(tx_keepalive);
        assert!(!drain_debounced(&rx, Duration::from_millis(50)));
    }

    #[test]
    fn a_disconnected_channel_with_no_events_returns_false_immediately() {
        let (tx, rx) = channel::<()>();
        drop(tx);
        assert!(!drain_debounced(&rx, Duration::from_millis(50)));
    }
}
