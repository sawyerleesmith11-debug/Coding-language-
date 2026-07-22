pub mod ast;
pub mod cse;
pub mod error;
pub mod fusion;
pub mod interner;
pub mod lexer;
pub mod parser;
pub mod purity;
pub mod resolve;
pub mod span;
pub mod typecheck;
pub mod where_info;

#[cfg(feature = "native")]
pub mod cache;
#[cfg(feature = "native")]
pub mod codegen;
#[cfg(feature = "native")]
pub mod inline;
#[cfg(feature = "native")]
pub mod jit_codegen;
#[cfg(feature = "native")]
pub mod profile;
#[cfg(feature = "native")]
pub mod watch;

/// Formats a diagnostic the way every kestrelc entry point (the CLI)
/// reports lex/parse errors: `file:line:col: message`, followed by the
/// offending source line and a `^` span underneath it — e.g.:
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
}
