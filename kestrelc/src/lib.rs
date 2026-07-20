pub mod ast;
pub mod lexer;
pub mod parser;
pub mod purity;
pub mod wasm_codegen;

#[cfg(feature = "native")]
pub mod cache;
#[cfg(feature = "native")]
pub mod codegen;

/// Runs the full front end (lex, parse, purity check) then the WASM
/// backend, collapsing every stage's error type into one formatted
/// String — this is the single entry point kestrelc-web's C-ABI shim
/// calls into, and it's also usable directly from Rust (e.g. the CLI's
/// `--wasm` path could use it, though it currently doesn't need to since
/// it already has the pieces inline).
pub fn compile_to_wasm_bytes(src: &str) -> Result<Vec<u8>, String> {
    let tokens = lexer::lex(src).map_err(|e| format!("{} (line {})", e.message, e.line))?;
    let program = parser::parse(tokens).map_err(|e| format!("{} (line {})", e.message, e.line))?;

    let purity_errors = purity::check_purity(&program);
    if !purity_errors.is_empty() {
        return Err(format!("Purity check failed:\n  {}", purity_errors.join("\n  ")));
    }

    if !program.iter().any(|f| f.name == "main") {
        return Err("No 'main' function found".to_string());
    }

    wasm_codegen::compile_to_wasm(&program).map_err(|e| e.0)
}
