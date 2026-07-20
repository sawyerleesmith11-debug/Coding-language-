pub mod ast;
pub mod error;
pub mod fusion;
pub mod interner;
pub mod lexer;
pub mod parser;
pub mod purity;
pub mod span;
pub mod typecheck;
pub mod wasm_codegen;
pub mod where_info;

#[cfg(feature = "native")]
pub mod cache;
#[cfg(feature = "native")]
pub mod codegen;
#[cfg(feature = "native")]
pub mod inline;
#[cfg(feature = "native")]
pub mod profile;

/// Formats a diagnostic the way every kestrelc entry point (the CLI, and
/// `compile_to_wasm_bytes` below) reports lex/parse errors: `file:line:col:
/// message`, followed by the offending source line and a `^` span
/// underneath it — e.g.:
///
/// ```text
/// fib.kes:3:12: Unexpected token 'RParen'
///   return x +;
///            ^
/// ```
///
/// Scope, honestly: this only covers lex and parse errors, since those are
/// the only stages that currently track a source position at all — purity
/// check, type check, and codegen errors are still message-only (a known
/// gap, not silently swept under this). `len` is the token's span width in
/// characters; it's clamped to at least 1 so a zero-length token (EOF)
/// still gets a visible caret.
pub fn format_diagnostic(src: &str, filename: &str, line: usize, col: usize, len: usize, message: &str) -> String {
    let line_text = src.lines().nth(line.saturating_sub(1)).unwrap_or("");
    let caret_len = len.max(1);
    let pointer = format!("{}{}", " ".repeat(col.saturating_sub(1)), "^".repeat(caret_len));
    format!("{filename}:{line}:{col}: {message}\n  {line_text}\n  {pointer}")
}

/// Runs the full front end (lex, parse, purity check) then the WASM
/// backend, collapsing every stage's error type into one formatted
/// String — this is the single entry point kestrelc-web's C-ABI shim
/// calls into, and it's also usable directly from Rust (e.g. the CLI's
/// `--wasm` path could use it, though it currently doesn't need to since
/// it already has the pieces inline).
pub fn compile_to_wasm_bytes(src: &str) -> Result<Vec<u8>, String> {
    let tokens = lexer::lex(src)
        .map_err(|e| format_diagnostic(src, "<input>", e.span.line, e.span.col, e.span.len, &e.message))?;
    let program = parser::parse(tokens)
        .map_err(|e| format_diagnostic(src, "<input>", e.span.line, e.span.col, e.span.len, &e.message))?;

    let purity_errors = purity::check_purity(&program);
    if !purity_errors.is_empty() {
        let msgs: Vec<String> = purity_errors
            .iter()
            .map(|e| format_diagnostic(src, "<input>", e.span.line, e.span.col, 1, &e.message))
            .collect();
        return Err(format!("Purity check failed:\n  {}", msgs.join("\n  ")));
    }

    let pmap_errors = purity::check_parallel_map(&program);
    if !pmap_errors.is_empty() {
        let msgs: Vec<String> = pmap_errors
            .iter()
            .map(|e| format_diagnostic(src, "<input>", e.span.line, e.span.col, 1, &e.message))
            .collect();
        return Err(format!("parallel_map() check failed:\n  {}", msgs.join("\n  ")));
    }

    let type_errors = typecheck::check_types(&program);
    if !type_errors.is_empty() {
        let msgs: Vec<String> = type_errors
            .iter()
            .map(|e| format_diagnostic(src, "<input>", e.span.line, e.span.col, 1, &e.message))
            .collect();
        return Err(format!("Type check failed:\n  {}", msgs.join("\n  ")));
    }

    if !program.iter().any(|f| &*f.name.resolve() == "main") {
        return Err("No 'main' function found".to_string());
    }

    let program = fusion::fuse_loops(&program);
    wasm_codegen::compile_to_wasm(&program).map_err(|e| {
        if e.span.line == 0 {
            e.message
        } else {
            format_diagnostic(src, "<input>", e.span.line, e.span.col, e.span.len.max(1), &e.message)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_diagnostic_renders_a_filename_line_col_header_and_a_caret_line() {
        let src = "fn main() {\n    let x = 5 $ 3;\n}\n";
        let out = format_diagnostic(src, "bad.kes", 2, 15, 1, "Unexpected character '$'");
        let expected = format!(
            "bad.kes:2:15: Unexpected character '$'\n  {}\n  {}^",
            "    let x = 5 $ 3;",
            " ".repeat(14) // col 15 -> 14 spaces before the caret
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn compile_to_wasm_bytes_reports_a_parse_error_with_a_caret_not_just_a_line_number() {
        let err = compile_to_wasm_bytes("fn main() { let x = ; }").unwrap_err();
        assert!(err.starts_with("<input>:1:21:"), "got: {err}");
        assert!(err.contains('^'), "expected a caret line, got: {err}");
    }
}
