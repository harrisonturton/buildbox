//! Parsing query expressions, as Bazel's query parser does.

use crate::lexer::{Token, TokenKind, is_reserved_word, scan};
use std::fmt;

/// A parsed query expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A target pattern, such as `//foo/...`, or a reference to a variable,
    /// such as `$x`.
    Target(String),
    Let { name: String, value: Box<Expr>, body: Box<Expr> },
    Set(Vec<String>),
    /// Operands joined by one operator, which applies left to right.
    Binary { op: SetOp, operands: Vec<Expr> },
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
    Int(i32),
}

/// The kind of argument a function takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    Expr,
    Word,
    Int,
}

/// A function a query can call: its name, the kinds of its arguments, and
/// how many of them must be given.
#[derive(Clone, Copy, Debug)]
pub struct Function {
    pub name: &'static str,
    pub args: &'static [ArgKind],
    pub mandatory: usize,
    /// Whether the last kind of argument repeats, as `rbuildfiles`' words
    /// do.
    pub repeats: bool,
}

impl Function {
    const fn new(name: &'static str, args: &'static [ArgKind]) -> Function {
        Function {
            name,
            args,
            mandatory: args.len(),
            repeats: false,
        }
    }

    const fn optional(name: &'static str, args: &'static [ArgKind], mandatory: usize) -> Function {
        Function {
            name,
            args,
            mandatory,
            repeats: false,
        }
    }
}

use ArgKind::{Expr as E, Int as I, Word as W};

/// The functions every query language has.
pub const DEFAULT_FUNCTIONS: &[Function] = &[
    Function::new("allpaths", &[E, E]),
    Function::new("attr", &[W, W, E]),
    Function::new("buildfiles", &[E]),
    Function::optional("deps", &[E, I], 1),
    Function::new("executables", &[E]),
    Function::new("filter", &[W, E]),
    Function::new("kind", &[W, E]),
    Function::new("labels", &[W, E]),
    Function::new("loadfiles", &[E]),
    Function::optional("rdeps", &[E, E, I], 2),
    Function::new("same_pkg_direct_rdeps", &[E]),
    Function::new("siblings", &[E]),
    Function::optional("some", &[E, I], 1),
    Function::new("somepath", &[E, E]),
    Function::new("tests", &[E]),
    Function::new("visible", &[E, E]),
];

/// The functions a query over a universe, `--universe_scope`, has besides.
pub const UNIVERSE_FUNCTIONS: &[Function] = &[
    Function::optional("allrdeps", &[E, I], 1),
    Function {
        name: "rbuildfiles",
        args: &[W],
        mandatory: 1,
        repeats: true,
    },
];

/// The function only `cquery` has.
pub const CQUERY_FUNCTIONS: &[Function] = &[Function::new("config", &[E, W])];

/// The functions only `aquery` has.
pub const AQUERY_FUNCTIONS: &[Function] = &[Function::new("inputs", &[W, E]), Function::new("mnemonic", &[W, E]), Function::new("outputs", &[W, E])];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// How deeply expressions may nest. Parsing, evaluating, printing and
/// dropping an expression each take stack in proportion to its depth, so
/// deeper ones are refused rather than exhausting it.
pub const MAX_DEPTH: usize = 500;

/// Parse a query expression whose functions are among `functions`.
pub fn parse(input: &str, functions: &[Function]) -> Result<Expr, ParseError> {
    let mut parser = Parser {
        tokens: scan(input)?,
        position: 0,
        functions,
        depth: 0,
    };
    let expr = parser.expression()?;
    if parser.token().kind != TokenKind::Eof {
        return Err(ParseError(format!(
            "unexpected token '{}' after query expression '{}'",
            parser.token(),
            truncate(&expr.to_string())
        )));
    }
    Ok(expr)
}

/// A query expression for messages, cut at 1000 characters as Bazel's are.
pub fn truncate(text: &str) -> String {
    const LIMIT: usize = 1000;
    const MARKER: &str = "[truncated]";
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let kept: String = text.chars().take(LIMIT - MARKER.len()).collect();
    format!("{kept}{MARKER}")
}

struct Parser<'a> {
    tokens: Vec<Token>,
    position: usize,
    functions: &'a [Function],
    /// How deeply the expression being parsed is nested.
    depth: usize,
}

impl Parser<'_> {
    fn token(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn advance(&mut self) {
        if self.token().kind != TokenKind::Eof {
            self.position += 1;
        }
    }

    fn consume(&mut self, kind: TokenKind) -> Result<String, ParseError> {
        if self.token().kind != kind {
            return Err(self.syntax_error());
        }
        let word = self.token().word.clone();
        self.advance();
        Ok(word)
    }

    /// Up to three tokens from `position`, for an error's context.
    fn context(&self, position: usize) -> String {
        let end = (self.tokens.len() - 1).min(position + 3);
        let tokens: Vec<String> = self.tokens[position..end].iter().map(Token::to_string).collect();
        tokens.join(" ")
    }

    fn syntax_error(&self) -> ParseError {
        if self.token().kind == TokenKind::Eof {
            return ParseError("premature end of input".to_string());
        }
        ParseError(format!("syntax error at '{}'", self.context(self.position)))
    }

    fn unknown_function(&self, position: usize) -> ParseError {
        let mut names: Vec<&str> = self.functions.iter().map(|function| function.name).collect();
        names.sort();
        ParseError(format!(
            "unknown function '{}' at '{}'; expected one of ['{}']",
            self.tokens[position],
            self.context(position),
            names.join("', '")
        ))
    }

    fn argument_count_error(&self, function: &Function, description: &str) -> ParseError {
        ParseError(format!("{description} arguments to function '{}' at '{}'", function.name, self.context(self.position)))
    }

    fn expression(&mut self) -> Result<Expr, ParseError> {
        let first = self.primary()?;
        self.binary_operator_tail(first)
    }

    fn binary_operator_tail(&mut self, first: Expr) -> Result<Expr, ParseError> {
        let Some(mut op) = set_op(self.token().kind) else {
            return Ok(first);
        };
        let mut operands = vec![first];
        let mut depth = self.depth;
        while let Some(next) = set_op(self.token().kind) {
            self.advance();
            if next != op {
                depth += 1;
                self.check_depth(depth)?;
                let joined = Expr::Binary { op, operands };
                operands = vec![joined];
                op = next;
            }
            operands.push(self.primary()?);
        }
        Ok(Expr::Binary { op, operands })
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        self.depth += 1;
        let primary = self.check_depth(self.depth).and_then(|_| self.nested_primary());
        self.depth -= 1;
        primary
    }

    fn check_depth(&self, depth: usize) -> Result<(), ParseError> {
        match depth > MAX_DEPTH {
            true => Err(ParseError(format!("query expression is nested more than {MAX_DEPTH} levels deep"))),
            false => Ok(()),
        }
    }

    fn nested_primary(&mut self) -> Result<Expr, ParseError> {
        match self.token().kind {
            TokenKind::Word => self.word_or_function(),
            TokenKind::Let => self.let_expression(),
            TokenKind::LParen => {
                self.advance();
                let expr = self.expression()?;
                self.consume(TokenKind::RParen)?;
                Ok(expr)
            }
            TokenKind::Set => self.set_expression(),
            _ => Err(self.syntax_error()),
        }
    }

    fn word_or_function(&mut self) -> Result<Expr, ParseError> {
        let position = self.position;
        let word = self.consume(TokenKind::Word)?;
        if self.token().kind != TokenKind::LParen {
            return target_literal(word);
        }
        let Some(function) = self.functions.iter().find(|function| function.name == word).copied() else {
            return Err(self.unknown_function(position));
        };
        let args = self.arguments(&function)?;
        Ok(Expr::Function { name: word, args })
    }

    fn arguments(&mut self, function: &Function) -> Result<Vec<Arg>, ParseError> {
        let mut args = Vec::new();
        let mut separator = TokenKind::LParen;
        let repeated = match function.repeats {
            true => usize::MAX,
            false => 0,
        };
        let kinds = function.args.iter().copied().chain(std::iter::repeat_n(*function.args.last().unwrap_or(&ArgKind::Word), repeated));
        for kind in kinds {
            if self.token().kind == TokenKind::RParen {
                if args.len() >= function.mandatory {
                    break;
                }
                return Err(self.argument_count_error(function, "too few"));
            }
            self.consume(separator)?;
            separator = TokenKind::Comma;
            if args.is_empty() && self.token().kind == TokenKind::RParen {
                return Err(self.argument_count_error(function, "too few"));
            }
            args.push(self.argument(kind)?);
        }
        if self.token().kind == TokenKind::Comma && !args.is_empty() {
            return Err(self.argument_count_error(function, "too many"));
        }
        self.consume(TokenKind::RParen)?;
        Ok(args)
    }

    fn argument(&mut self, kind: ArgKind) -> Result<Arg, ParseError> {
        match kind {
            ArgKind::Expr => Ok(Arg::Expr(self.expression()?)),
            ArgKind::Word => Ok(Arg::Word(self.consume(TokenKind::Word)?)),
            ArgKind::Int => {
                let word = self.consume(TokenKind::Word)?;
                match word.parse() {
                    Ok(value) => Ok(Arg::Int(value)),
                    Err(_) => Err(ParseError(format!("expected an integer literal: '{word}'"))),
                }
            }
        }
    }

    fn let_expression(&mut self) -> Result<Expr, ParseError> {
        self.consume(TokenKind::Let)?;
        let name = self.consume(TokenKind::Word)?;
        self.consume(TokenKind::Equals)?;
        let value = self.expression()?;
        self.consume(TokenKind::In)?;
        let body = self.expression()?;
        Ok(Expr::Let {
            name,
            value: Box::new(value),
            body: Box::new(body),
        })
    }

    fn set_expression(&mut self) -> Result<Expr, ParseError> {
        self.advance();
        self.consume(TokenKind::LParen)?;
        let mut words = Vec::new();
        while self.token().kind == TokenKind::Word {
            let word = self.consume(TokenKind::Word)?;
            target_literal(word.clone())?;
            words.push(word);
        }
        self.consume(TokenKind::RParen)?;
        Ok(Expr::Set(words))
    }
}

fn set_op(kind: TokenKind) -> Option<SetOp> {
    if !kind.is_binary_operator() {
        return None;
    }
    match kind {
        TokenKind::Plus | TokenKind::Union => Some(SetOp::Union),
        TokenKind::Caret | TokenKind::Intersect => Some(SetOp::Intersect),
        _ => Some(SetOp::Except),
    }
}

fn target_literal(word: String) -> Result<Expr, ParseError> {
    match word.chars().next() {
        Some(first @ ('-' | '*')) => Err(ParseError(format!("target literal must not begin with ({first}): {word}"))),
        _ => Ok(Expr::Target(word)),
    }
}

impl Expr {
    /// Whether the expression is a call of `somepath`, whose result
    /// queries print in the path's order.
    pub fn is_top_level_somepath(&self) -> bool {
        matches!(self, Expr::Function { name, .. } if name == "somepath")
    }

    /// The target patterns the expression names, other than variables, as
    /// a universe inferred from it has them.
    pub fn target_patterns(&self, patterns: &mut Vec<String>) {
        match self {
            Expr::Target(pattern) if !pattern.starts_with('$') => patterns.push(pattern.clone()),
            Expr::Target(_) => {}
            Expr::Let { value, body, .. } => {
                value.target_patterns(patterns);
                body.target_patterns(patterns);
            }
            Expr::Set(words) => patterns.extend(words.iter().cloned()),
            Expr::Binary { operands, .. } => operands.iter().for_each(|operand| operand.target_patterns(patterns)),
            Expr::Function { args, .. } => {
                for arg in args {
                    if let Arg::Expr(expr) = arg {
                        expr.target_patterns(patterns);
                    }
                }
            }
        }
    }
}

impl fmt::Display for SetOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SetOp::Union => "+",
            SetOp::Intersect => "^",
            SetOp::Except => "-",
        })
    }
}

/// Expressions as Bazel prints them in messages.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Target(pattern) => f.write_str(&quoted_pattern(pattern)),
            Expr::Let { name, value, body } => write!(f, "let {name} = {value} in {body}"),
            Expr::Set(words) => write!(f, "set({})", words.join(" ")),
            Expr::Binary { op, operands } => {
                let operands: Vec<String> = operands.iter().map(Expr::to_string).collect();
                write!(f, "({})", operands.join(&format!(" {op} ")))
            }
            Expr::Function { name, args } => {
                let args: Vec<String> = args.iter().map(Arg::to_string).collect();
                write!(f, "{name}({})", args.join(", "))
            }
        }
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Expr(expr) => write!(f, "{expr}"),
            Arg::Word(word) => write!(f, "'{word}'"),
            Arg::Int(value) => write!(f, "{value}"),
        }
    }
}

/// A target pattern as Bazel prints it: quoted if it couldn't be read
/// back otherwise.
fn quoted_pattern(pattern: &str) -> String {
    let needs_quoting = is_reserved_word(pattern)
        || pattern.is_empty()
        || pattern.starts_with('-')
        || pattern.starts_with('*')
        || pattern.chars().any(|c| " \"#&'()+,;<=>?[]{|}".contains(c));
    if !needs_quoting {
        return pattern.to_string();
    }
    let quote = if pattern.contains('"') { '\'' } else { '"' };
    format!("{quote}{pattern}{quote}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> Result<Expr, ParseError> {
        parse(text, DEFAULT_FUNCTIONS)
    }

    #[test]
    fn parses_operators_left_to_right() {
        let expr = query("//a/... - //a/b:all + //c").unwrap();
        assert_eq!(expr.to_string(), "((//a/... - //a/b:all) + //c)");
        assert_eq!(query("//a + //b + //c").unwrap().to_string(), "(//a + //b + //c)");
        assert_eq!(query("//my-target").unwrap(), Expr::Target("//my-target".into()));
    }

    #[test]
    fn refuses_expressions_nested_too_deeply() {
        let deep = |depth: usize| format!("{}//a{}", "deps(".repeat(depth), ")".repeat(depth));
        assert!(query(&deep(MAX_DEPTH - 1)).is_ok());
        assert!(query(&deep(MAX_DEPTH + 1)).unwrap_err().0.contains("nested more than"));
        let alternating: String = (0..MAX_DEPTH + 1).map(|_| " + //b - //c").collect();
        assert!(query(&format!("//a{alternating}")).unwrap_err().0.contains("nested more than"));
    }

    #[test]
    fn parses_functions() {
        assert_eq!(query("deps(//foo, 2)").unwrap().to_string(), "deps(//foo, 2)");
        assert_eq!(query("kind('cc_.* rule', deps(//foo))").unwrap().to_string(), "kind('cc_.* rule', deps(//foo))");
        assert_eq!(query("let x = //a in $x ^ //b").unwrap().to_string(), "let x = //a in ($x ^ //b)");
        assert_eq!(query("set(//a //b)").unwrap().to_string(), "set(//a //b)");
        assert!(parse("rbuildfiles(a/BUILD, b/BUILD)", UNIVERSE_FUNCTIONS).is_ok());
    }

    #[test]
    fn reports_errors_as_bazel_does() {
        let error = |text: &str| query(text).unwrap_err().0;
        assert_eq!(error("deps(//:a"), "premature end of input");
        assert_eq!(error("kind(rule, //:a, //:b)"), "too many arguments to function 'kind' at ', //:b )'");
        assert_eq!(error("kind(rule)"), "too few arguments to function 'kind' at ')'");
        assert_eq!(error("deps()"), "too few arguments to function 'deps' at ')'");
        assert!(error("nofunc(//:a)").starts_with("unknown function 'nofunc' at 'nofunc ( //:a'; expected one of ['allpaths', 'attr',"));
        assert_eq!(error("//a )"), "unexpected token ')' after query expression '//a'");
        assert_eq!(error("deps(//a, x)"), "expected an integer literal: 'x'");
        assert_eq!(error("'unclosed"), "unclosed quotation");
        assert_eq!(error(""), "premature end of input");
        assert_eq!(error("//a +"), "premature end of input");
    }
}
