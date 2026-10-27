//! Parsing query expressions, as Bazel's query parser does.
//!
//! An expression is kept as a list of its parts, each referring to the
//! parts it's made of by their index, and is parsed, walked and printed
//! with stacks of its own rather than by recursion, so that expressions
//! nested arbitrarily deeply work as shallow ones do.

use crate::lexer::{Token, TokenKind, is_reserved_word, scan};
use std::fmt;

/// A parsed query expression: its parts, the last being the whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    nodes: Vec<Node>,
    root: ExprId,
}

/// A part of an expression, by its index among the expression's parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(u32);

/// A part of a query expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A target pattern, such as `//foo/...`, or a reference to a variable,
    /// such as `$x`.
    Target(String),
    Let { name: String, value: ExprId, body: ExprId },
    Set(Vec<String>),
    /// Operands joined by one operator, which applies left to right.
    Binary { op: SetOp, operands: Vec<ExprId> },
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
    Expr(ExprId),
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

/// Parse a query expression whose functions are among `functions`.
pub fn parse(input: &str, functions: &[Function]) -> Result<Expr, ParseError> {
    let mut parser = Parser {
        tokens: scan(input)?,
        position: 0,
        functions,
        nodes: Vec::new(),
        frames: Vec::new(),
    };
    let root = parser.expression()?;
    let expr = Expr { nodes: parser.nodes, root };
    if parser.tokens[parser.position].kind != TokenKind::Eof {
        return Err(ParseError(format!(
            "unexpected token '{}' after query expression '{}'",
            parser.tokens[parser.position],
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


/// A parser: the tokens, the parts parsed so far, and the constructs
/// whose parts are being parsed, innermost last.
struct Parser<'a> {
    tokens: Vec<Token>,
    position: usize,
    functions: &'a [Function],
    nodes: Vec<Node>,
    frames: Vec<Frame>,
}

/// A construct whose parts are being parsed.
enum Frame {
    /// An expression: its operands so far, and their operator once one is
    /// met. Operators apply left to right, so a different one makes what
    /// came before its first operand.
    Expr { operands: Vec<ExprId>, op: Option<SetOp> },
    /// `( expression )`, awaiting its `)`.
    Paren,
    /// `let name = value in body`, awaiting its value.
    LetValue { name: String },
    /// `let name = value in body`, awaiting its body.
    LetBody { name: String, value: ExprId },
    /// A function's call, awaiting its arguments.
    Call { name: String, function: Function, args: Vec<Arg> },
}

/// What the parser does next.
enum Step {
    /// Parse a primary expression: a pattern, call, `let`, parenthesized
    /// expression or `set`.
    Primary,
    /// A primary expression was parsed.
    Parsed(ExprId),
    /// An expression was parsed: give it to the construct it's part of.
    Completed(ExprId),
    /// Parse the next argument of the call being parsed.
    Argument,
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

    fn add(&mut self, node: Node) -> ExprId {
        self.nodes.push(node);
        ExprId(self.nodes.len() as u32 - 1)
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

    /// Parse a whole expression, the constructs it's made of kept on the
    /// parser's own stack.
    fn expression(&mut self) -> Result<ExprId, ParseError> {
        self.frames.push(Frame::Expr { operands: Vec::new(), op: None });
        let mut step = Step::Primary;
        loop {
            step = match step {
                Step::Primary => self.primary()?,
                Step::Parsed(id) => self.operand(id)?,
                Step::Argument => self.argument()?,
                Step::Completed(id) => match self.frames.pop() {
                    None => return Ok(id),
                    Some(frame) => self.completed(frame, id)?,
                },
            };
        }
    }

    /// Begin an expression inside the construct being parsed.
    fn nested_expression(&mut self) -> Step {
        self.frames.push(Frame::Expr { operands: Vec::new(), op: None });
        Step::Primary
    }

    fn primary(&mut self) -> Result<Step, ParseError> {
        match self.token().kind {
            TokenKind::Word => self.word_or_call(),
            TokenKind::Let => {
                self.advance();
                let name = self.consume(TokenKind::Word)?;
                self.consume(TokenKind::Equals)?;
                self.frames.push(Frame::LetValue { name });
                Ok(self.nested_expression())
            }
            TokenKind::LParen => {
                self.advance();
                self.frames.push(Frame::Paren);
                Ok(self.nested_expression())
            }
            TokenKind::Set => self.set_expression(),
            _ => Err(self.syntax_error()),
        }
    }

    fn word_or_call(&mut self) -> Result<Step, ParseError> {
        let position = self.position;
        let word = self.consume(TokenKind::Word)?;
        if self.token().kind != TokenKind::LParen {
            let node = target_literal(word)?;
            return Ok(Step::Parsed(self.add(node)));
        }
        let Some(function) = self.functions.iter().find(|function| function.name == word).copied() else {
            return Err(self.unknown_function(position));
        };
        self.frames.push(Frame::Call { name: word, function, args: Vec::new() });
        Ok(Step::Argument)
    }

    fn set_expression(&mut self) -> Result<Step, ParseError> {
        self.advance();
        self.consume(TokenKind::LParen)?;
        let mut words = Vec::new();
        while self.token().kind == TokenKind::Word {
            let word = self.consume(TokenKind::Word)?;
            target_literal(word.clone())?;
            words.push(word);
        }
        self.consume(TokenKind::RParen)?;
        Ok(Step::Parsed(self.add(Node::Set(words))))
    }

    /// Add a primary expression to the expression being parsed, and go on
    /// to the next operand if an operator follows.
    fn operand(&mut self, id: ExprId) -> Result<Step, ParseError> {
        let next = set_op(self.token().kind);
        let Some(Frame::Expr { operands, op }) = self.frames.last_mut() else {
            return Err(ParseError("an operand outside an expression".to_string()));
        };
        operands.push(id);
        let Some(next) = next else {
            let (operands, op) = (std::mem::take(operands), op.take());
            self.frames.pop();
            let id = match op {
                None => operands.first().copied().ok_or_else(|| ParseError("an expression without operands".to_string()))?,
                Some(op) => self.add(Node::Binary { op, operands }),
            };
            return Ok(Step::Completed(id));
        };
        if let Some(current) = *op {
            if current != next {
                let joined = Node::Binary { op: current, operands: std::mem::take(operands) };
                self.nodes.push(joined);
                let joined = ExprId(self.nodes.len() as u32 - 1);
                if let Some(Frame::Expr { operands, .. }) = self.frames.last_mut() {
                    operands.push(joined);
                }
            }
        }
        if let Some(Frame::Expr { op, .. }) = self.frames.last_mut() {
            *op = Some(next);
        }
        self.advance();
        Ok(Step::Primary)
    }

    /// Give a parsed expression to the construct it's part of.
    fn completed(&mut self, frame: Frame, id: ExprId) -> Result<Step, ParseError> {
        match frame {
            Frame::Expr { .. } => Err(ParseError("an expression completed inside another".to_string())),
            Frame::Paren => {
                self.consume(TokenKind::RParen)?;
                Ok(Step::Parsed(id))
            }
            Frame::LetValue { name } => {
                self.consume(TokenKind::In)?;
                self.frames.push(Frame::LetBody { name, value: id });
                Ok(self.nested_expression())
            }
            Frame::LetBody { name, value } => Ok(Step::Parsed(self.add(Node::Let { name, value, body: id }))),
            Frame::Call { name, function, mut args } => {
                args.push(Arg::Expr(id));
                self.frames.push(Frame::Call { name, function, args });
                Ok(Step::Argument)
            }
        }
    }

    /// Parse the next argument of the call being parsed, or its end. A
    /// function's last kind of argument may repeat.
    fn argument(&mut self) -> Result<Step, ParseError> {
        let Some(Frame::Call { function, args, .. }) = self.frames.last() else {
            return Err(ParseError("an argument outside a call".to_string()));
        };
        let function = *function;
        let index = args.len();
        let kind = match (function.args.get(index), function.repeats) {
            (Some(kind), _) => Some(*kind),
            (None, true) => function.args.last().copied(),
            (None, false) => None,
        };
        let Some(kind) = kind else {
            return self.end_of_call(&function, index);
        };
        if self.token().kind == TokenKind::RParen {
            if index >= function.mandatory {
                return self.end_of_call(&function, index);
            }
            return Err(self.argument_count_error(&function, "too few"));
        }
        let separator = if index == 0 { TokenKind::LParen } else { TokenKind::Comma };
        self.consume(separator)?;
        if index == 0 && self.token().kind == TokenKind::RParen {
            return Err(self.argument_count_error(&function, "too few"));
        }
        let arg = match kind {
            ArgKind::Expr => return Ok(self.nested_expression()),
            ArgKind::Word => Arg::Word(self.consume(TokenKind::Word)?),
            ArgKind::Int => {
                let word = self.consume(TokenKind::Word)?;
                match word.parse() {
                    Ok(value) => Arg::Int(value),
                    Err(_) => return Err(ParseError(format!("expected an integer literal: '{word}'"))),
                }
            }
        };
        if let Some(Frame::Call { args, .. }) = self.frames.last_mut() {
            args.push(arg);
        }
        Ok(Step::Argument)
    }

    fn end_of_call(&mut self, function: &Function, count: usize) -> Result<Step, ParseError> {
        if self.token().kind == TokenKind::Comma && count > 0 {
            return Err(self.argument_count_error(function, "too many"));
        }
        self.consume(TokenKind::RParen)?;
        let Some(Frame::Call { name, args, .. }) = self.frames.pop() else {
            return Err(ParseError("the end of a call outside one".to_string()));
        };
        Ok(Step::Parsed(self.add(Node::Function { name, args })))
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

fn target_literal(word: String) -> Result<Node, ParseError> {
    match word.chars().next() {
        Some(first @ ('-' | '*')) => Err(ParseError(format!("target literal must not begin with ({first}): {word}"))),
        _ => Ok(Node::Target(word)),
    }
}

impl Expr {
    /// The whole expression.
    pub fn root(&self) -> ExprId {
        self.root
    }

    /// A part of the expression.
    pub fn node(&self, id: ExprId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// The expression's parts, each before those it's made of, left to
    /// right.
    pub fn preorder(&self) -> Vec<ExprId> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut pending = vec![self.root];
        while let Some(id) = pending.pop() {
            order.push(id);
            let start = pending.len();
            pending.extend(self.children(id));
            pending[start..].reverse();
        }
        order
    }

    /// The parts a part is made of, left to right.
    fn children(&self, id: ExprId) -> Vec<ExprId> {
        match self.node(id) {
            Node::Target(_) | Node::Set(_) => Vec::new(),
            Node::Let { value, body, .. } => vec![*value, *body],
            Node::Binary { operands, .. } => operands.clone(),
            Node::Function { args, .. } => args.iter().filter_map(|arg| match arg {
                Arg::Expr(id) => Some(*id),
                _ => None,
            }).collect(),
        }
    }

    /// Whether the expression is a call of `somepath`, whose result
    /// queries print in the path's order.
    pub fn is_top_level_somepath(&self) -> bool {
        matches!(self.node(self.root), Node::Function { name, .. } if name == "somepath")
    }

    /// The target patterns the expression names, other than variables, as
    /// a universe inferred from it has them.
    pub fn target_patterns(&self, patterns: &mut Vec<String>) {
        for id in self.preorder() {
            match self.node(id) {
                Node::Target(pattern) if !pattern.starts_with('$') => patterns.push(pattern.clone()),
                Node::Set(words) => patterns.extend(words.iter().cloned()),
                _ => {}
            }
        }
    }

    /// A part of the expression as Bazel prints it in messages.
    pub fn text(&self, id: ExprId) -> String {
        let mut out = String::new();
        let mut pending = vec![Piece::Part(id)];
        while let Some(piece) = pending.pop() {
            match piece {
                Piece::Text(text) => out.push_str(&text),
                Piece::Part(id) => {
                    let start = pending.len();
                    pending.extend(self.pieces(id));
                    pending[start..].reverse();
                }
            }
        }
        out
    }

    /// What a part prints as, its own parts left to be printed.
    fn pieces(&self, id: ExprId) -> Vec<Piece> {
        match self.node(id) {
            Node::Target(pattern) => vec![Piece::Text(quoted_pattern(pattern))],
            Node::Let { name, value, body } => vec![
                Piece::Text(format!("let {name} = ")),
                Piece::Part(*value),
                Piece::Text(" in ".to_string()),
                Piece::Part(*body),
            ],
            Node::Set(words) => vec![Piece::Text(format!("set({})", words.join(" ")))],
            Node::Binary { op, operands } => {
                let mut pieces = vec![Piece::Text("(".to_string())];
                for (index, operand) in operands.iter().enumerate() {
                    if index > 0 {
                        pieces.push(Piece::Text(format!(" {op} ")));
                    }
                    pieces.push(Piece::Part(*operand));
                }
                pieces.push(Piece::Text(")".to_string()));
                pieces
            }
            Node::Function { name, args } => {
                let mut pieces = vec![Piece::Text(format!("{name}("))];
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        pieces.push(Piece::Text(", ".to_string()));
                    }
                    pieces.push(match arg {
                        Arg::Expr(id) => Piece::Part(*id),
                        Arg::Word(word) => Piece::Text(format!("'{word}'")),
                        Arg::Int(value) => Piece::Text(value.to_string()),
                    });
                }
                pieces.push(Piece::Text(")".to_string()));
                pieces
            }
        }
    }
}

/// A piece of an expression being printed: text, or a part still to print.
enum Piece {
    Text(String),
    Part(ExprId),
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
        f.write_str(&self.text(self.root))
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
        let target = query("//my-target").unwrap();
        assert_eq!(target.node(target.root()), &Node::Target("//my-target".into()));
    }

    #[test]
    fn parses_prints_and_drops_expressions_nested_arbitrarily_deeply() {
        const DEPTH: usize = 200_000;
        let calls = format!("{}//a{}", "deps(".repeat(DEPTH), ")".repeat(DEPTH));
        let expr = query(&calls).unwrap();
        assert_eq!(expr.to_string(), calls);
        let mut patterns = Vec::new();
        expr.target_patterns(&mut patterns);
        assert_eq!(patterns, ["//a"]);

        let parens = format!("{}//a + //b{}", "(".repeat(DEPTH), ")".repeat(DEPTH));
        assert_eq!(query(&parens).unwrap().to_string(), "(//a + //b)");
        let lets: String = (0..DEPTH).map(|_| "let x = //a in ").collect();
        assert!(query(&format!("{lets}$x")).is_ok());
        let alternating: String = (0..DEPTH).map(|_| " + //b - //c").collect();
        let expr = query(&format!("//a{alternating}")).unwrap();
        assert!(expr.to_string().starts_with(&"(".repeat(100)));
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
