mod ast;
mod codegen;
mod lexer;
mod parser;
mod purity;

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: kestrelc <file.kes>");
        return ExitCode::FAILURE;
    }
    let path = &args[1];
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kestrelc: can't read '{path}': {e}");
            return ExitCode::FAILURE;
        }
    };

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

    let src_path = Path::new(path);
    let stem = src_path.file_stem().unwrap().to_string_lossy();
    let obj_path = format!("{stem}.o");
    let out_path = stem.to_string();

    if let Err(e) = fs::write(&obj_path, &object_bytes) {
        eprintln!("kestrelc: failed to write '{obj_path}': {e}");
        return ExitCode::FAILURE;
    }

    let link_status = Command::new("cc")
        .arg(&obj_path)
        .arg("-o")
        .arg(&out_path)
        .status();

    match link_status {
        Ok(status) if status.success() => {
            println!("kestrelc: wrote ./{out_path}");
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
