// Compile-and-run orchestration for kestrelc-devtool's /run endpoint --
// mirrors kestrelc/src/watch.rs's try_jit-then-AOT-fallback structure,
// calling the same public kestrelc library functions watch.rs already
// does (watch.rs's own try_jit/report_error are private to that module,
// so this is a fresh call to the same underlying pipeline, not a reuse
// of watch.rs's glue -- see the design doc). Real, separately-timed
// compile_ms/run_ms is the one thing watch.rs doesn't already expose
// (it only reports one combined "finished in Xms").

use kestrelc::error::KestrelcError;
use kestrelc::{jit_codegen, lexer, parser, purity, resolve, typecheck};
use std::process::Command;
use std::time::Instant;

pub struct RunResult {
    pub engine: &'static str,
    pub ok: bool,
    pub compile_ms: f64,
    pub run_ms: f64,
    pub output: String,
    pub error: Option<String>,
}

impl RunResult {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"engine\":\"{}\",\"ok\":{},\"compile_ms\":{},\"run_ms\":{},\"output\":\"{}\",\"error\":{}}}",
            self.engine,
            self.ok,
            self.compile_ms,
            self.run_ms,
            json_escape(&self.output),
            match &self.error {
                Some(e) => format!("\"{}\"", json_escape(e)),
                None => "null".to_string(),
            },
        )
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn run_source(src: &str) -> RunResult {
    let tokens = match lexer::lex(src) {
        Ok(t) => t,
        Err(e) => return compile_error(&e, src),
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => return compile_error(&e, src),
    };

    let fns = resolve::build_fn_table(&program);
    let structs = resolve::build_struct_table(&program);
    let resolve_errors = resolve::resolve(&program, &fns, &structs);
    if let Some(e) = resolve_errors.first() {
        return compile_error(e, src);
    }
    let purity_errors = purity::check_purity(&program, &fns);
    if let Some(e) = purity_errors.first() {
        return compile_error(e, src);
    }
    let pmap_errors = purity::check_parallel_map(&program, &fns);
    if let Some(e) = pmap_errors.first() {
        return compile_error(e, src);
    }
    let type_errors = typecheck::check_types(&program, &fns);
    if let Some(e) = type_errors.first() {
        return compile_error(e, src);
    }
    if !program.fns.iter().any(|f| f.name == kestrelc::interner::well_known::main()) {
        return RunResult {
            engine: "jit",
            ok: false,
            compile_ms: 0.0,
            run_ms: 0.0,
            output: String::new(),
            error: Some("kestrelc: No 'main' function found".to_string()),
        };
    }

    match jit_codegen::check_jit_supported(&program) {
        Ok(()) => run_via_jit(&program),
        Err(_) => run_via_aot(src),
    }
}

fn compile_error(e: &KestrelcError, src: &str) -> RunResult {
    let message = if e.span.line == 0 {
        format!("kestrelc: {}", e.message)
    } else {
        format!(
            "kestrelc: {}",
            kestrelc::format_diagnostic(src, "<devtool>", e.span.line, e.span.col, e.span.len.max(1), &e.message)
        )
    };
    RunResult { engine: "jit", ok: false, compile_ms: 0.0, run_ms: 0.0, output: String::new(), error: Some(message) }
}

fn run_via_jit(program: &kestrelc::ast::Program) -> RunResult {
    let compile_start = Instant::now();
    let mut cg = match jit_codegen::JitCodegen::new_capturing() {
        Ok(c) => c,
        Err(e) => {
            return RunResult { engine: "jit", ok: false, compile_ms: 0.0, run_ms: 0.0, output: String::new(), error: Some(e.message) }
        }
    };
    if let Err(e) = cg.compile_program(program) {
        return RunResult { engine: "jit", ok: false, compile_ms: 0.0, run_ms: 0.0, output: String::new(), error: Some(e.message) };
    }
    let compile_ms = compile_start.elapsed().as_secs_f64() * 1000.0;

    jit_codegen::begin_capture();
    let run_start = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cg.finish_and_run()));
    let run_ms = run_start.elapsed().as_secs_f64() * 1000.0;
    let output = jit_codegen::take_captured_output();

    match result {
        Ok(Ok(_)) => RunResult { engine: "jit", ok: true, compile_ms, run_ms, output, error: None },
        Ok(Err(e)) => RunResult { engine: "jit", ok: false, compile_ms, run_ms, output, error: Some(e.message) },
        Err(panic_payload) => {
            let msg = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic in JIT backend".to_string());
            RunResult { engine: "jit", ok: false, compile_ms, run_ms, output, error: Some(msg) }
        }
    }
}

fn run_via_aot(src: &str) -> RunResult {
    let dir = std::env::temp_dir().join(format!("kestrelc_devtool_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src_path = dir.join("prog.kes");
    if let Err(e) = std::fs::write(&src_path, src) {
        return RunResult {
            engine: "aot",
            ok: false,
            compile_ms: 0.0,
            run_ms: 0.0,
            output: String::new(),
            error: Some(format!("kestrelc-devtool: couldn't write temp file: {e}")),
        };
    }
    let kestrelc_exe = match std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("kestrelc.exe"))) {
        Some(p) if p.exists() => p,
        _ => {
            return RunResult {
                engine: "aot",
                ok: false,
                compile_ms: 0.0,
                run_ms: 0.0,
                output: String::new(),
                error: Some(
                    "kestrelc-devtool: couldn't find kestrelc.exe next to this binary -- build kestrelc first \
                     (cargo build --release -p kestrelc) and copy it alongside kestrelc-devtool's binary."
                        .to_string(),
                ),
            }
        }
    };

    let compile_start = Instant::now();
    let compile_output = match Command::new(&kestrelc_exe).arg(&src_path).current_dir(&dir).output() {
        Ok(o) => o,
        Err(e) => {
            return RunResult {
                engine: "aot",
                ok: false,
                compile_ms: 0.0,
                run_ms: 0.0,
                output: String::new(),
                error: Some(format!("kestrelc-devtool: failed to invoke kestrelc: {e}")),
            }
        }
    };
    let compile_ms = compile_start.elapsed().as_secs_f64() * 1000.0;
    if !compile_output.status.success() {
        return RunResult {
            engine: "aot",
            ok: false,
            compile_ms,
            run_ms: 0.0,
            output: String::new(),
            error: Some(String::from_utf8_lossy(&compile_output.stderr).into_owned()),
        };
    }

    let bin_path = dir.join("prog");
    let run_start = Instant::now();
    let run_output = Command::new(&bin_path).output();
    let run_ms = run_start.elapsed().as_secs_f64() * 1000.0;
    match run_output {
        Ok(o) => RunResult {
            engine: "aot",
            ok: true,
            compile_ms,
            run_ms,
            output: String::from_utf8_lossy(&o.stdout).into_owned(),
            error: None,
        },
        Err(e) => RunResult {
            engine: "aot",
            ok: false,
            compile_ms,
            run_ms,
            output: String::new(),
            error: Some(format!("kestrelc-devtool: failed to run compiled binary: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jit_eligible_program_runs_via_the_jit_engine_with_real_output() {
        let result = run_source("fn main() { print(\"hi\", 42); }");
        assert_eq!(result.engine, "jit");
        assert!(result.ok, "expected ok, got error: {:?}", result.error);
        assert_eq!(result.output, "hi 42\n");
        assert!(result.compile_ms >= 0.0);
        assert!(result.run_ms >= 0.0);
        assert!(result.error.is_none());
    }

    #[test]
    fn a_program_with_a_compile_error_reports_ok_false_with_the_real_diagnostic() {
        let result = run_source("fn main() { let x = ; }");
        assert!(!result.ok);
        assert!(result.error.as_ref().unwrap().contains("Unexpected"), "got: {:?}", result.error);
    }

    #[test]
    fn to_json_produces_valid_well_shaped_json() {
        let result = RunResult {
            engine: "jit",
            ok: true,
            compile_ms: 1.5,
            run_ms: 0.25,
            output: "hi \"there\"\n".to_string(),
            error: None,
        };
        let json = result.to_json();
        assert!(json.contains("\"engine\":\"jit\""));
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"compile_ms\":1.5"));
        assert!(json.contains("hi \\\"there\\\"\\n"));
        assert!(json.contains("\"error\":null"));
    }

    // The AOT fallback path (a_jit_ineligible_program_falls_back...)
    // needs a real kestrelc.exe sitting next to whatever binary
    // current_exe() resolves to -- not true for `cargo test`'s test
    // binaries (they live in target/debug/deps/, no kestrelc.exe
    // nearby). Covered by manual verification instead (see the design
    // doc's testing plan / plan.md Task 4 Step 2) rather than forcing a
    // brittle path-guessing test.
}
