// A source position, consolidated: `line`/`col`/`len` used to be three
// separate fields, copy-pasted across lexer::Token, lexer::LexError,
// parser::ParseError, ast::Stmt (once per variant), ast::Fn, and
// ast::CheckError — one struct instead, threaded everywhere those were.
// `len` is the span's character width (not derivable from line/col
// alone — those only mark the start), used to size a caret underline in
// format_diagnostic.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
    pub len: usize,
}

impl Span {
    pub fn new(line: usize, col: usize, len: usize) -> Self {
        Span { line, col, len }
    }
}
