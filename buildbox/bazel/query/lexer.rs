//! Splitting query expressions into tokens, as Bazel's query lexer does.

use crate::parse::ParseError;
use std::fmt;

/// What a token is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Word,
    Eof,
    Comma,
    Equals,
    LParen,
    Minus,
    Plus,
    RParen,
    Caret,
    In,
    Let,
    Set,
    Intersect,
    Except,
    Union,
}

impl TokenKind {
    /// How Bazel names the token in syntax errors.
    fn pretty_name(self) -> &'static str {
        match self {
            TokenKind::Word => "word",
            TokenKind::Eof => "EOF",
            TokenKind::Comma => ",",
            TokenKind::Equals => "=",
            TokenKind::LParen => "(",
            TokenKind::Minus => "-",
            TokenKind::Plus => "+",
            TokenKind::RParen => ")",
            TokenKind::Caret => "^",
            TokenKind::In => "in",
            TokenKind::Let => "let",
            TokenKind::Set => "set",
            TokenKind::Intersect => "intersect",
            TokenKind::Except => "except",
            TokenKind::Union => "union",
        }
    }

    /// Whether the token is a binary set operator.
    pub(crate) fn is_binary_operator(self) -> bool {
        matches!(
            self,
            TokenKind::Intersect | TokenKind::Caret | TokenKind::Union | TokenKind::Plus | TokenKind::Except | TokenKind::Minus
        )
    }
}

/// The keywords, which aren't words.
const KEYWORDS: [TokenKind; 6] = [TokenKind::In, TokenKind::Let, TokenKind::Set, TokenKind::Intersect, TokenKind::Except, TokenKind::Union];

/// Whether `word` is a keyword, which a target pattern must be quoted to
/// be.
pub(crate) fn is_reserved_word(word: &str) -> bool {
    KEYWORDS.iter().any(|keyword| keyword.pretty_name() == word)
}

/// A token, and for a word, its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub word: String,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            TokenKind::Word => f.write_str(&self.word),
            kind => f.write_str(kind.pretty_name()),
        }
    }
}

impl Token {
    fn of(kind: TokenKind) -> Token {
        Token { kind, word: String::new() }
    }

    fn word(word: &str) -> Token {
        let keyword = KEYWORDS.iter().find(|keyword| keyword.pretty_name() == word);
        match keyword {
            Some(kind) => Token::of(*kind),
            None => Token {
                kind: TokenKind::Word,
                word: word.to_string(),
            },
        }
    }
}

/// The tokens of `input`, ending with [`TokenKind::Eof`].
pub(crate) fn scan(input: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        let c = chars[pos];
        pos += 1;
        let token = match c {
            '(' => Token::of(TokenKind::LParen),
            ')' => Token::of(TokenKind::RParen),
            ',' => Token::of(TokenKind::Comma),
            '+' => Token::of(TokenKind::Plus),
            '-' => Token::of(TokenKind::Minus),
            '=' => Token::of(TokenKind::Equals),
            '^' => Token::of(TokenKind::Caret),
            '\n' | ' ' | '\t' | '\r' => continue,
            '\'' | '"' => quoted_word(&chars, &mut pos, c)?,
            _ => Token::word(&scan_word(&chars, &mut pos)),
        };
        tokens.push(token);
    }
    tokens.push(Token::of(TokenKind::Eof));
    Ok(tokens)
}

/// A quoted word, whose opening quote has been read: everything up to the
/// same quote.
fn quoted_word(chars: &[char], pos: &mut usize, quote: char) -> Result<Token, ParseError> {
    let start = *pos;
    while *pos < chars.len() {
        let c = chars[*pos];
        *pos += 1;
        if c == quote {
            let word: String = chars[start..*pos - 1].iter().collect();
            return Ok(Token {
                kind: TokenKind::Word,
                word,
            });
        }
    }
    Err(ParseError("unclosed quotation".to_string()))
}

/// An unquoted word, whose first character has been read. `+` ends it,
/// unless it names a canonical repository, as `@@rules_foo+//...` does.
fn scan_word(chars: &[char], pos: &mut usize) -> String {
    let start = *pos - 1;
    let canonical = chars[start] == '@' && chars.get(*pos) == Some(&'@');
    while let Some(&c) = chars.get(*pos) {
        let continues = match c {
            '+' => canonical,
            c if c.is_ascii_alphanumeric() => true,
            '*' | '/' | '@' | '.' | '-' | '_' | ':' | '$' | '~' | '[' | ']' => true,
            c => !c.is_ascii(),
        };
        if !continues {
            break;
        }
        *pos += 1;
    }
    chars[start..*pos].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(input: &str) -> Vec<String> {
        scan(input).unwrap().iter().map(Token::to_string).collect()
    }

    #[test]
    fn scans_words_operators_and_keywords() {
        assert_eq!(texts("deps(//a:b-c, 2)"), ["deps", "(", "//a:b-c", ",", "2", ")", "EOF"]);
        assert_eq!(texts("//a + -//b"), ["//a", "+", "-", "//b", "EOF"]);
        assert_eq!(texts("@@rules_foo+//x except //y"), ["@@rules_foo+//x", "except", "//y", "EOF"]);
        assert_eq!(texts("'a b' \"c'd\""), ["a b", "c'd", "EOF"]);
        assert!(scan("'open").is_err());
    }
}
