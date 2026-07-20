// Direct port of kestrel.js's lex() — same token set, same rules.
// See docs/SYNTAX.md for the grammar this implements.

use crate::error::{ErrorKind, KestrelcError};
use crate::interner::{self, Symbol};
use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tok {
    Number(i64),
    Str(Symbol),
    Ident(Symbol),
    Fn,
    Pure,
    Let,
    If,
    Else,
    While,
    Where,
    Print,
    Return,
    True,
    False,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Colon,
    Lt,
    Gt,
    Eq,
    Bang,
    Dot,
    Arrow,
    EqEq,
    NotEq,
    LtEq,
    GtEq,
    AndAnd,
    OrOr,
    Eof,
}

#[derive(Debug, Clone, Copy)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub fn lex(src: &str) -> Result<Vec<Token>, KestrelcError> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut tokens = Vec::new();

    while i < chars.len() {
        let c = chars[i];

        if c == '\n' {
            line += 1;
            col = 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            col += 1;
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        let (start, start_line, start_col) = (i, line, col);
        // `col` no longer tracks whitespace/comments below this point — every
        // branch advances `i` and `col` together, one char at a time, so the
        // two stay in lockstep for the span-length math (`i - start`) at the end.

        if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
                col += 1;
            }
            let text: String = chars[start..i].iter().collect();
            // kestrelc supports integers only for now (see kestrelc/README.md).
            let value: i64 = text.parse().map_err(|_| KestrelcError::new(
                ErrorKind::Lex,
                format!("kestrelc only supports integer literals so far, found '{text}'"),
                Span::new(start_line, start_col, i - start),
            ))?;
            tokens.push(Token { tok: Tok::Number(value), span: Span::new(start_line, start_col, i - start) });
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
                col += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let tok = match word.as_str() {
                "fn" => Tok::Fn,
                "pure" => Tok::Pure,
                "let" => Tok::Let,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "where" => Tok::Where,
                "print" => Tok::Print,
                "return" => Tok::Return,
                "true" => Tok::True,
                "false" => Tok::False,
                _ => Tok::Ident(interner::intern(&word)),
            };
            tokens.push(Token { tok, span: Span::new(start_line, start_col, i - start) });
            continue;
        }
        if c == '"' {
            i += 1;
            col += 1;
            let str_start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
                col += 1;
            }
            let s: String = chars[str_start..i].iter().collect();
            i += 1; // closing quote
            col += 1;
            tokens.push(Token { tok: Tok::Str(interner::intern(&s)), span: Span::new(start_line, start_col, i - start) });
            continue;
        }

        let two: Option<Tok> = if i + 1 < chars.len() {
            match (c, chars[i + 1]) {
                ('=', '=') => Some(Tok::EqEq),
                ('!', '=') => Some(Tok::NotEq),
                ('<', '=') => Some(Tok::LtEq),
                ('>', '=') => Some(Tok::GtEq),
                ('-', '>') => Some(Tok::Arrow),
                ('&', '&') => Some(Tok::AndAnd),
                ('|', '|') => Some(Tok::OrOr),
                _ => None,
            }
        } else {
            None
        };
        if let Some(t) = two {
            i += 2;
            col += 2;
            tokens.push(Token { tok: t, span: Span::new(start_line, start_col, 2) });
            continue;
        }

        let one = match c {
            '+' => Some(Tok::Plus),
            '-' => Some(Tok::Minus),
            '*' => Some(Tok::Star),
            '/' => Some(Tok::Slash),
            '%' => Some(Tok::Percent),
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            '{' => Some(Tok::LBrace),
            '}' => Some(Tok::RBrace),
            '[' => Some(Tok::LBracket),
            ']' => Some(Tok::RBracket),
            ';' => Some(Tok::Semi),
            ',' => Some(Tok::Comma),
            ':' => Some(Tok::Colon),
            '<' => Some(Tok::Lt),
            '>' => Some(Tok::Gt),
            '=' => Some(Tok::Eq),
            '!' => Some(Tok::Bang),
            '.' => Some(Tok::Dot),
            _ => None,
        };
        match one {
            Some(t) => {
                i += 1;
                col += 1;
                tokens.push(Token { tok: t, span: Span::new(start_line, start_col, 1) });
            }
            None => {
                return Err(KestrelcError::new(
                    ErrorKind::Lex,
                    format!("Unexpected character '{c}'"),
                    Span::new(start_line, start_col, 1),
                ));
            }
        }
    }

    tokens.push(Token { tok: Tok::Eof, span: Span::new(line, col, 0) });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_line_and_column_across_a_multi_line_program() {
        let tokens = lex("fn main() {\n    let x = 5;\n}\n").unwrap();
        // 'let' starts the second line, indented 4 spaces.
        let let_tok = tokens.iter().find(|t| t.tok == Tok::Let).unwrap();
        assert_eq!(let_tok.span.line, 2);
        assert_eq!(let_tok.span.col, 5);
        assert_eq!(let_tok.span.len, 3);
    }

    #[test]
    fn token_length_matches_the_source_text_it_covers() {
        let tokens = lex("let count = 123;").unwrap();
        let number = tokens.iter().find(|t| matches!(t.tok, Tok::Number(_))).unwrap();
        assert_eq!(number.span.col, 13);
        assert_eq!(number.span.len, 3); // "123"
    }

    #[test]
    fn unexpected_character_error_points_at_the_exact_column() {
        let err = lex("let x = 5 $ 3;").unwrap_err();
        assert_eq!(err.span.line, 1);
        assert_eq!(err.span.col, 11); // the '$'
        assert_eq!(err.span.len, 1);
    }

    #[test]
    fn identical_identifiers_intern_to_the_same_symbol() {
        let tokens = lex("let x = x + x;").unwrap();
        let idents: Vec<Symbol> =
            tokens.iter().filter_map(|t| if let Tok::Ident(s) = t.tok { Some(s) } else { None }).collect();
        assert_eq!(idents.len(), 3);
        assert!(idents.iter().all(|&s| s == idents[0]), "every 'x' should intern to the same Symbol");
    }
}
