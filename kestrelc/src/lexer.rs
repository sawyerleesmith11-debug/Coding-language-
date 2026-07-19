// Direct port of kestrel.js's lex() — same token set, same rules.
// See docs/SYNTAX.md for the grammar this implements.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Number(i64),
    Str(String),
    Ident(String),
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

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
}

pub struct LexError {
    pub message: String,
    pub line: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut tokens = Vec::new();

    let push = |tokens: &mut Vec<Token>, tok: Tok, line: usize| tokens.push(Token { tok, line });

    while i < chars.len() {
        let c = chars[i];

        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            // kestrelc supports integers only for now (see kestrelc/README.md).
            let value: i64 = text.parse().map_err(|_| LexError {
                message: format!("kestrelc only supports integer literals so far, found '{text}'"),
                line,
            })?;
            push(&mut tokens, Tok::Number(value), line);
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
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
                _ => Tok::Ident(word),
            };
            push(&mut tokens, tok, line);
            continue;
        }
        if c == '"' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            i += 1; // closing quote
            push(&mut tokens, Tok::Str(s), line);
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
            push(&mut tokens, t, line);
            i += 2;
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
                push(&mut tokens, t, line);
                i += 1;
            }
            None => {
                return Err(LexError {
                    message: format!("Unexpected character '{c}'"),
                    line,
                });
            }
        }
    }

    push(&mut tokens, Tok::Eof, line);
    Ok(tokens)
}
