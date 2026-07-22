// One error type for the whole compiler, instead of a different struct
// per stage (lexer::LexError, parser::ParseError, ast::CheckError,
// codegen::CodegenError, wasm_codegen::WasmError — five shapes that all
// carried the same two things: a message and a position). `kind` is a
// small, discriminant-only enum (no payload — see ErrorKind, an "empty
// enum is 1 byte" tag), everything else lives in the shared struct. Lets
// every stage's errors flow through the same reporting path in main.rs
// and lib.rs instead of five near-identical printing blocks.

use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Lex,
    Parse,
    Resolve,
    Purity,
    ParallelMap,
    Type,
    Codegen,
}

impl ErrorKind {
    /// The header line main.rs prints above a list of errors of this
    /// kind (lex/parse/codegen errors are reported one at a time, so
    /// this only actually shows up for the three checker kinds — see
    /// main.rs's report_many/report_one).
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Lex => "Lex error",
            ErrorKind::Parse => "Parse error",
            ErrorKind::Resolve => "Name resolution failed",
            ErrorKind::Purity => "Purity check failed",
            ErrorKind::ParallelMap => "parallel_map() check failed",
            ErrorKind::Type => "Type check failed",
            ErrorKind::Codegen => "Codegen error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct KestrelcError {
    pub kind: ErrorKind,
    pub message: String,
    pub span: Span,
}

impl KestrelcError {
    pub fn new(kind: ErrorKind, message: String, span: Span) -> Self {
        KestrelcError { kind, message, span }
    }

    /// For an error with no meaningful source position (an internal
    /// compiler-level failure — a Cranelift/module error, not "your
    /// program did X" — see codegen.rs's many `.map_err` sites that
    /// wrap linker/ISA errors). A zero-length span at (0, 0) is a
    /// sentinel main.rs/lib.rs's formatting recognizes and renders
    /// without a caret line, same spirit as fusion.rs's `SYNTHESIZED`
    /// span for AST nodes with no real source position either.
    pub fn internal(kind: ErrorKind, message: String) -> Self {
        KestrelcError { kind, message, span: Span::new(0, 0, 0) }
    }
}

/// A non-fatal diagnostic: printed, but never a reason to fail the
/// build or change an exit code, unlike every `KestrelcError`. A
/// deliberately separate type rather than a `severity` field on
/// `KestrelcError` — every existing call site across the compiler
/// already treats "a non-empty `Vec<KestrelcError>`" as "the build
/// failed" (`.is_empty()` checks, early returns); folding warnings into
/// that same type would mean auditing every one of those sites to make
/// sure a warning-only result doesn't accidentally trip them. No `kind`
/// enum yet (unlike `ErrorKind`) — only one warning exists so far (see
/// `resolve::check_size_warnings`); add one if/when a second kind does.
#[derive(Debug, Clone)]
pub struct KestrelcWarning {
    pub message: String,
    pub span: Span,
}

impl KestrelcWarning {
    pub fn new(message: String, span: Span) -> Self {
        KestrelcWarning { message, span }
    }
}
