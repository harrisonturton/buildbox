//! Parsing query expressions.

use std::fmt;

/// A parsed query expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A target pattern, such as `//foo/...`.
    Pattern(String),
    Variable(String),
    Let { name: String, value: Box<Expr>, body: Box<Expr> },
    Set(Vec<String>),
    Binary { op: SetOp, left: Box<Expr>, right: Box<Expr> },
    Function { name: String, args: Vec<Arg> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

/// A function argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arg {
    Expr(Expr),
    Word(String),
    Int(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    LParen,
    RParen,
    Comma,
    Equals,
    Plus,
    Minus,
    Caret,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Word(word) => write!(f, "'{word}'"),
            Token::LParen => write!(f, "'('"),
            Token::RParen => write!(f, "')'"),
            Token::Comma => write!(f, "','"),
            Token::Equals => write!(f, "'='"),
            Token::Plus => write!(f, "'+'"),
            Token::Minus => write!(f, "'-'"),
            Token::Caret => write!(f, "'^'"),
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || "*/@.-_:$~[]+!#%&<>?{}|;".contains(c)
}

fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            '=' => {
                chars.next();
                tokens.push(Token::Equals);
            }
            '^' => {
                chars.next();
                tokens.push(Token::Caret);
            }
            '"' | '\'' => {
                chars.next();
                let mut word = String::new();
                let mut closed = false;
                for (_, next) in chars.by_ref() {
                    if next == c {
                        closed = true;
                        break;
                    }
                    word.push(next);
                }
                if !closed {
                    return Err(ParseError(format!("unclosed quotation in query expression at position {start}")));
                }
                tokens.push(Token::Word(word));
            }
            _ if is_word_char(c) => {
                let mut word = String::new();
                while let Some(&(_, next)) = chars.peek() {
                    if !is_word_char(next) {
                        break;
                    }
                    word.push(next);
                    chars.next();
                }
                match word.as_str() {
                    "+" => tokens.push(Token::Plus),
                    "-" => tokens.push(Token::Minus),
                    _ => tokens.push(Token::Word(word)),
                }
            }
            other => return Err(ParseError(format!("unexpected character '{other}' in query expression"))),
        }
    }
    Ok(tokens)
}

/// The functions a query can call, and the kinds of argument each takes.
fn signature(name: &str) -> Option<&'static [ArgKind]> {
    use ArgKind::*;
    let signature: &'static [ArgKind] = match name {
        "allpaths" | "somepath" | "visible" | "rdeps_universe" => &[Expr, Expr],
        "attr" => &[Word, Word, Expr],
        "buildfiles" | "loadfiles" | "siblings" | "tests" | "same_pkg_direct_rdeps" | "rbuildfiles_of" => &[Expr],
        "deps" | "allrdeps" | "some" => &[Expr, OptionalInt],
        "rdeps" => &[Expr, Expr, OptionalInt],
        "filter" | "kind" => &[Word, Expr],
        "labels" => &[Word, Expr],
        "rbuildfiles" => &[Words],
        _ => return None,
    };
    Some(signature)
}

#[derive(Clone, Copy, Debug)]
enum ArgKind {
    Expr,
    Word,
    OptionalInt,
    Words,
}

/// Parse a query expression.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(ParseError("syntax error: empty query expression".to_string()));
    }
    let mut parser = Parser { tokens, position: 0 };
    let expr = parser.expr()?;
    if let Some(token) = parser.peek() {
        return Err(ParseError(format!("syntax error at {token}: expected end of query")));
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position += 1;
        token
    }

    fn expect(&mut self, expected: Token) -> Result<(), ParseError> {
        match self.next() {
            Some(token) if token == expected => Ok(()),
            Some(token) => Err(ParseError(format!("syntax error at {token}: expected {expected}"))),
            None => Err(ParseError(format!("syntax error: expected {expected} at end of query"))),
        }
    }

    fn word(&mut self) -> Result<String, ParseError> {
        match self.next() {
            Some(Token::Word(word)) => Ok(word),
            Some(token) => Err(ParseError(format!("syntax error at {token}: expected a word"))),
            None => Err(ParseError("syntax error: unexpected end of query".to_string())),
        }
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        if self.peek() == Some(&Token::Word("let".to_string())) {
            return self.let_expr();
        }
        let mut left = self.primary()?;
        while let Some(op) = self.binary_op() {
            let right = self.primary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn binary_op(&mut self) -> Option<SetOp> {
        let op = match self.peek()? {
            Token::Plus => SetOp::Union,
            Token::Minus => SetOp::Except,
            Token::Caret => SetOp::Intersect,
            Token::Word(word) => match word.as_str() {
                "union" => SetOp::Union,
                "except" => SetOp::Except,
                "intersect" => SetOp::Intersect,
                _ => return None,
            },
            _ => return None,
        };
        self.position += 1;
        Some(op)
    }

    fn let_expr(&mut self) -> Result<Expr, ParseError> {
        self.position += 1;
        let name = self.word()?;
        self.expect(Token::Equals)?;
        let value = self.expr()?;
        if self.word()? != "in" {
            return Err(ParseError("syntax error: expected 'in' after let binding".to_string()));
        }
        let body = self.expr()?;
        Ok(Expr::Let {
            name,
            value: Box::new(value),
            body: Box::new(body),
        })
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Token::LParen) => {
                let expr = self.expr()?;
                self.expect(Token::RParen)?;
                Ok(expr)
            }
            Some(Token::Word(word)) => self.word_expr(word),
            Some(token) => Err(ParseError(format!("syntax error at {token}"))),
            None => Err(ParseError("syntax error: unexpected end of query".to_string())),
        }
    }

    fn word_expr(&mut self, word: String) -> Result<Expr, ParseError> {
        if let Some(name) = word.strip_prefix('$') {
            return Ok(Expr::Variable(name.to_string()));
        }
        if self.peek() != Some(&Token::LParen) {
            return Ok(Expr::Pattern(word));
        }
        if word == "set" {
            self.position += 1;
            let mut words = Vec::new();
            while let Some(Token::Word(_)) = self.peek() {
                words.push(self.word()?);
            }
            self.expect(Token::RParen)?;
            return Ok(Expr::Set(words));
        }
        let Some(signature) = signature(&word) else {
            return Err(ParseError(format!("syntax error: unknown function '{word}'")));
        };
        self.position += 1;
        let args = self.args(&word, signature)?;
        Ok(Expr::Function { name: word, args })
    }

    fn args(&mut self, function: &str, signature: &[ArgKind]) -> Result<Vec<Arg>, ParseError> {
        let mut args = Vec::new();
        for (index, kind) in signature.iter().enumerate() {
            if let ArgKind::OptionalInt = kind {
                if self.peek() == Some(&Token::Comma) {
                    self.position += 1;
                    let word = self.word()?;
                    let depth = word
                        .parse()
                        .map_err(|_| ParseError(format!("{function}: expected an integer, got '{word}'")))?;
                    args.push(Arg::Int(depth));
                }
                continue;
            }
            if index > 0 {
                self.expect(Token::Comma)?;
            }
            match kind {
                ArgKind::Expr => args.push(Arg::Expr(self.expr()?)),
                ArgKind::Word => args.push(Arg::Word(self.word()?)),
                ArgKind::Words => {
                    while let Some(Token::Word(_)) = self.peek() {
                        args.push(Arg::Word(self.word()?));
                        if self.peek() == Some(&Token::Comma) {
                            self.position += 1;
                        }
                    }
                }
                ArgKind::OptionalInt => {}
            }
        }
        self.expect(Token::RParen)?;
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str) -> Box<Expr> {
        Box::new(Expr::Pattern(text.to_string()))
    }

    #[test]
    fn parses_patterns_and_operators() {
        assert_eq!(parse("//foo:bar").unwrap(), Expr::Pattern("//foo:bar".into()));
        assert_eq!(
            parse("//a/... - //a/b:all + //c").unwrap(),
            Expr::Binary {
                op: SetOp::Union,
                left: Box::new(Expr::Binary {
                    op: SetOp::Except,
                    left: pattern("//a/..."),
                    right: pattern("//a/b:all"),
                }),
                right: pattern("//c"),
            }
        );
        assert_eq!(parse("//my-target").unwrap(), Expr::Pattern("//my-target".into()));
    }

    #[test]
    fn parses_functions() {
        assert_eq!(
            parse("deps(//foo, 2)").unwrap(),
            Expr::Function {
                name: "deps".into(),
                args: vec![Arg::Expr(Expr::Pattern("//foo".into())), Arg::Int(2)],
            }
        );
        assert_eq!(
            parse("kind('cc_.* rule', deps(//foo))").unwrap(),
            Expr::Function {
                name: "kind".into(),
                args: vec![
                    Arg::Word("cc_.* rule".into()),
                    Arg::Expr(Expr::Function {
                        name: "deps".into(),
                        args: vec![Arg::Expr(Expr::Pattern("//foo".into()))],
                    }),
                ],
            }
        );
        assert!(matches!(parse("let x = //a in $x ^ //b").unwrap(), Expr::Let { .. }));
        assert!(matches!(parse("set(//a //b)").unwrap(), Expr::Set(words) if words.len() == 2));
    }

    #[test]
    fn rejects_bad_syntax() {
        for bad in ["", "deps(", "nosuchfunction(//a)", "//a +", "'unclosed"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
