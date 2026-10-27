//! Evaluating query expressions over any graph of targets: the package
//! graph `query` reads, or the configured graph of `cquery` and `aquery`.
//!
//! An environment says what its nodes are, which targets a pattern names
//! and how nodes depend on each other; the functions every query language
//! has are evaluated here in terms of that, as Bazel's are.

use crate::functions;
use crate::nodes::NodeSet;
use crate::parse::{Arg, Expr, Function, SetOp, truncate};
use crate::universe::TargetRef;
use async_trait::async_trait;
use bazel_label::Label;
use bazel_packages::{RuleClass, RuleClassRef};
use futures::future::BoxFuture;
use intern::Symbol;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

/// Why a query failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub message: String,
    failed_in: FailedIn,
}

/// The expression an error is in, as Bazel describes a failed query.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FailedIn {
    /// None: the error is reported as it is, as a target pattern's is.
    Nothing,
    /// The evaluation, but no expression of it in particular.
    Evaluation,
    /// The expression being evaluated, once the evaluator knows which.
    Pending,
    /// This expression, and whether it's the whole query.
    Expression { text: String, top_level: bool },
}

impl QueryError {
    /// An error reported as it is.
    pub fn new(message: impl Into<String>) -> QueryError {
        QueryError {
            message: message.into(),
            failed_in: FailedIn::Nothing,
        }
    }

    /// An error in evaluating the query, which Bazel doesn't attribute to
    /// an expression of it.
    pub fn in_evaluation(message: impl Into<String>) -> QueryError {
        QueryError {
            message: message.into(),
            failed_in: FailedIn::Evaluation,
        }
    }

    /// An error in evaluating an expression, such as a function's call,
    /// which Bazel reports as that expression's failure.
    pub fn in_expression(message: impl Into<String>) -> QueryError {
        QueryError {
            message: message.into(),
            failed_in: FailedIn::Pending,
        }
    }

    fn attributed(mut self, expr: &Expr, top: &Expr) -> QueryError {
        if self.failed_in == FailedIn::Pending {
            self.failed_in = FailedIn::Expression {
                text: truncate(&expr.to_string()),
                top_level: std::ptr::eq(expr, top),
            };
        }
        self
    }

    /// The error as Bazel reports a query's failure: naming the expression
    /// it was in, if any.
    pub fn describe(&self) -> String {
        match &self.failed_in {
            FailedIn::Nothing => self.message.clone(),
            FailedIn::Pending | FailedIn::Evaluation => format!("Evaluation failed: {}", self.message),
            FailedIn::Expression { text, top_level: true } => format!("Evaluation of query \"{text}\" failed: {}", self.message),
            FailedIn::Expression { text, top_level: false } => {
                format!("Evaluation of subquery \"{text}\" failed (did you want to use --keep_going?): {}", self.message)
            }
        }
    }
}

impl From<skyframe::SkyError> for QueryError {
    fn from(error: skyframe::SkyError) -> QueryError {
        QueryError::new(error.message())
    }
}

/// The graph a query is evaluated over.
#[async_trait]
pub trait QueryEnvironment: Sync + Sized {
    /// What the query's expressions denote sets of.
    type Node: Clone + Eq + Hash + Ord + Send + Sync + Debug + 'static;

    /// The functions queries over this environment can call.
    fn functions(&self) -> Vec<Function>;

    /// The label of the target a node is of.
    fn label(&self, node: &Self::Node) -> Label;

    /// The nodes a target pattern names.
    async fn targets_matching(&self, pattern: &str) -> Result<NodeSet<Self::Node>, QueryError>;

    /// The nodes `node` depends on directly, as the query follows
    /// dependencies.
    async fn fwd_deps(&self, node: &Self::Node) -> Result<Vec<Self::Node>, QueryError>;

    /// The nodes `roots` depend on, transitively, to `depth` steps if
    /// given, and the roots.
    async fn transitive_closure(&self, roots: NodeSet<Self::Node>, depth: Option<u32>) -> Result<NodeSet<Self::Node>, QueryError> {
        functions::breadth_first(self, roots, depth).await
    }

    /// The target a node is of.
    async fn target_of(&self, node: &Self::Node) -> Result<TargetRef, QueryError>;

    /// The schema of a rule's class.
    async fn rule_class(&self, class: &RuleClassRef) -> Result<Arc<RuleClass>, QueryError>;

    /// The values of a rule's attribute as `attr()` matches them, one for
    /// each value it may have; `None` if the rule has no such attribute.
    async fn attr_strings(&self, node: &Self::Node, attr: Symbol) -> Result<Option<Vec<String>>, QueryError>;

    /// The nodes a rule's attribute names, as `labels()` gives them.
    async fn attr_nodes(&self, node: &Self::Node, attr: Symbol) -> Result<Vec<Self::Node>, QueryError>;

    /// The nodes of the targets in the packages of `nodes`.
    async fn siblings(&self, nodes: &NodeSet<Self::Node>) -> Result<NodeSet<Self::Node>, QueryError>;

    /// Whether `target` is visible to `from`.
    async fn is_visible(&self, from: &Self::Node, target: &Self::Node) -> Result<bool, QueryError>;

    /// Evaluate a call of a function only this environment has.
    async fn call(&self, cx: &Context<'_, Self>, name: &str, args: &[Arg]) -> Result<NodeSet<Self::Node>, QueryError> {
        let _ = (cx, args);
        Err(QueryError::in_expression(format!("{name}() isn't supported")))
    }
}

/// The values of the variables `let` binds.
pub type Vars<N> = HashMap<String, NodeSet<N>>;

/// What an expression is evaluated in: its environment, the query it's
/// part of, and the variables bound around it.
pub struct Context<'a, E: QueryEnvironment> {
    pub env: &'a E,
    top: &'a Expr,
    vars: Vars<E::Node>,
}

impl<'a, E: QueryEnvironment> Context<'a, E> {
    /// Evaluate `expr`, a part of `top`.
    pub fn eval<'b>(&'b self, expr: &'b Expr) -> BoxFuture<'b, Result<NodeSet<E::Node>, QueryError>> {
        Box::pin(async move {
            let result = match expr {
                Expr::Target(pattern) => self.target(pattern).await,
                Expr::Let { name, value, body } => self.let_expression(name, value, body).await,
                Expr::Set(patterns) => self.set(patterns).await,
                Expr::Binary { op, operands } => self.binary(*op, operands).await,
                Expr::Function { name, args } => functions::call(self, name, args).await,
            };
            result.map_err(|error| error.attributed(expr, self.top))
        })
    }

    async fn set(&self, patterns: &[String]) -> Result<NodeSet<E::Node>, QueryError> {
        let mut nodes = NodeSet::new();
        for pattern in patterns {
            nodes.extend(self.target(pattern).await?);
        }
        Ok(nodes)
    }

    async fn target(&self, pattern: &str) -> Result<NodeSet<E::Node>, QueryError> {
        let Some(name) = pattern.strip_prefix('$') else {
            return self.env.targets_matching(pattern).await;
        };
        match self.vars.get(name) {
            Some(value) => Ok(value.clone()),
            None => Err(QueryError::in_expression(format!("undefined variable '{name}'"))),
        }
    }

    async fn let_expression(&self, name: &str, value: &Expr, body: &Expr) -> Result<NodeSet<E::Node>, QueryError> {
        if !is_variable_name(name) {
            return Err(QueryError::in_expression(format!("invalid variable name '{name}' in let expression")));
        }
        let value = self.eval(value).await?;
        let mut vars = self.vars.clone();
        vars.insert(name.to_string(), value);
        let inner = Context {
            env: self.env,
            top: self.top,
            vars,
        };
        inner.eval(body).await
    }

    async fn binary(&self, op: SetOp, operands: &[Expr]) -> Result<NodeSet<E::Node>, QueryError> {
        let Some((first, rest)) = operands.split_first() else {
            return Err(QueryError::in_evaluation("a set operation needs operands".to_string()));
        };
        let mut result = self.eval(first).await?;
        for operand in rest {
            let value = self.eval(operand).await?;
            result = match op {
                SetOp::Union => result.union(value),
                SetOp::Intersect => result.intersection(&value),
                SetOp::Except => result.difference(&value),
            };
        }
        Ok(result)
    }
}

/// Whether `name` is a variable's name: an identifier, as in C.
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Evaluate a query expression in an environment.
pub async fn evaluate<E: QueryEnvironment>(env: &E, expr: &Expr) -> Result<NodeSet<E::Node>, QueryError> {
    let cx = Context {
        env,
        top: expr,
        vars: Vars::new(),
    };
    cx.eval(expr).await
}
