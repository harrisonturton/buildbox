//! Evaluating query expressions over any graph of targets: the package
//! graph `query` reads, or the configured graph of `cquery` and `aquery`.
//!
//! An environment says what its nodes are, which targets a pattern names
//! and how nodes depend on each other; the functions every query language
//! has are evaluated here in terms of that, as Bazel's are.
//!
//! Expressions are evaluated with stacks of the evaluator's own, of what's
//! left to do and of the values found, rather than by recursion, so that
//! expressions nested arbitrarily deeply evaluate as shallow ones do.

use crate::functions;
use crate::nodes::NodeSet;
use crate::parse::{Arg, Expr, ExprId, Function, Node, SetOp, truncate};
use crate::universe::TargetRef;
use async_trait::async_trait;
use bazel_label::Label;
use bazel_packages::{RuleClass, RuleClassRef};
use intern::Symbol;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

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

    /// The error, as one in the part `id` of `expr` unless it's known to
    /// be in a part of it.
    fn attributed(mut self, expr: &Expr, id: ExprId) -> QueryError {
        if self.failed_in == FailedIn::Pending {
            self.failed_in = FailedIn::Expression {
                text: truncate(&expr.text(id)),
                top_level: id == expr.root(),
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

/// The arguments of a function's call, its expressions evaluated: the
/// environment, and each argument expression's value, or why it couldn't
/// be found. Expressions after one that failed aren't evaluated.
pub struct Context<'a, E: QueryEnvironment> {
    pub env: &'a E,
    values: Mutex<Vec<Option<Result<NodeSet<E::Node>, QueryError>>>>,
}

impl<E: QueryEnvironment> Context<'_, E> {
    /// The value of the expression that's argument `index`, taken: each is
    /// asked for once.
    pub(crate) fn take(&self, index: usize) -> Result<NodeSet<E::Node>, QueryError> {
        let mut values = self.values.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match values.get_mut(index).and_then(Option::take) {
            Some(value) => value,
            None => Err(QueryError::in_evaluation(format!("argument {} wasn't evaluated", index + 1))),
        }
    }
}

/// The variables `let` binds around an expression, innermost first.
type Vars<N> = Option<Arc<Binding<N>>>;

/// A variable `let` binds, and those bound around it.
struct Binding<N> {
    name: String,
    value: NodeSet<N>,
    outer: Vars<N>,
}

impl<N> Drop for Binding<N> {
    /// Drop the bindings only this one holds without recursion, so that a
    /// long chain of them can't exhaust the stack.
    fn drop(&mut self) {
        let mut outer = self.outer.take();
        while let Some(binding) = outer {
            outer = match Arc::try_unwrap(binding) {
                Ok(mut binding) => binding.outer.take(),
                Err(_) => None,
            };
        }
    }
}

fn lookup<'a, N>(vars: &'a Vars<N>, name: &str) -> Option<&'a NodeSet<N>> {
    let mut binding = vars.as_deref();
    while let Some(current) = binding {
        if current.name == name {
            return Some(&current.value);
        }
        binding = current.outer.as_deref();
    }
    None
}

/// What's left to do in evaluating an expression. Each task that takes
/// values takes them from the top of the stack of values, and leaves its
/// own there.
enum Task<'a, N> {
    /// Evaluate a part of the expression, leaving its value.
    Eval { id: ExprId, vars: Vars<N> },
    /// With the value of a `let`'s value, evaluate its body.
    Bind { id: ExprId, name: &'a str, body: ExprId, vars: Vars<N> },
    /// Attribute an error in the value on top to the part `id`.
    Attribute { id: ExprId },
    /// Combine the value on top, of operand `index - 1`, with those before
    /// it, then evaluate the next operand.
    Combine {
        id: ExprId,
        op: SetOp,
        operands: &'a [ExprId],
        index: usize,
        result: Option<NodeSet<N>>,
        vars: Vars<N>,
    },
    /// Evaluate a call's argument expressions in turn, the value on top
    /// being argument `pending`'s, then call the function.
    Call {
        id: ExprId,
        name: &'a str,
        args: &'a [Arg],
        pending: Option<usize>,
        values: Vec<Option<Result<NodeSet<N>, QueryError>>>,
        vars: Vars<N>,
    },
}

/// Counts steps of work that may never wait, and lets the runtime run
/// other tasks every so often. A long query then shares its thread, and
/// stops when its command is cancelled, which only happens where a task
/// yields.
#[derive(Default)]
pub(crate) struct Budget {
    steps: u32,
}

impl Budget {
    const STEPS: u32 = 256;

    /// Count a step, yielding once every [`Budget::STEPS`].
    pub(crate) async fn step(&mut self) {
        self.steps += 1;
        if self.steps % Self::STEPS == 0 {
            YieldNow(false).await;
        }
    }
}

/// A future that's pending once, waking itself, then ready.
struct YieldNow(bool);

impl std::future::Future for YieldNow {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.0 {
            return std::task::Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
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
    let evaluator = Evaluator {
        env,
        expr,
        tasks: Vec::new(),
        values: Vec::new(),
        patterns: std::collections::HashMap::new(),
        budget: Budget::default(),
    };
    evaluator.run().await
}

/// An expression's evaluation: what's left to do, and the values found.
struct Evaluator<'a, E: QueryEnvironment> {
    env: &'a E,
    expr: &'a Expr,
    tasks: Vec<Task<'a, E::Node>>,
    values: Vec<Result<NodeSet<E::Node>, QueryError>>,
    /// What each target pattern named, once resolved: as in Bazel, a
    /// pattern is resolved once however often a query names it.
    patterns: std::collections::HashMap<&'a str, Result<NodeSet<E::Node>, QueryError>>,
    budget: Budget,
}

impl<'a, E: QueryEnvironment> Evaluator<'a, E> {
    async fn run(mut self) -> Result<NodeSet<E::Node>, QueryError> {
        self.tasks.push(Task::Eval { id: self.expr.root(), vars: None });
        while let Some(task) = self.tasks.pop() {
            self.budget.step().await;
            match task {
                Task::Eval { id, vars } => self.eval(id, vars).await,
                Task::Bind { id, name, body, vars } => self.bind(id, name, body, vars),
                Task::Attribute { id } => {
                    let value = self.pop();
                    self.values.push(value.map_err(|error| error.attributed(self.expr, id)));
                }
                Task::Combine { id, op, operands, index, result, vars } => self.combine(id, op, operands, index, result, vars),
                Task::Call { id, name, args, pending, values, vars } => self.call(id, name, args, pending, values, vars).await,
            }
        }
        self.pop()
    }

    fn pop(&mut self) -> Result<NodeSet<E::Node>, QueryError> {
        self.values.pop().unwrap_or_else(|| Err(QueryError::in_evaluation("an expression had no value")))
    }

    fn push(&mut self, id: ExprId, value: Result<NodeSet<E::Node>, QueryError>) {
        self.values.push(value.map_err(|error| error.attributed(self.expr, id)));
    }

    async fn eval(&mut self, id: ExprId, vars: Vars<E::Node>) {
        let expr = self.expr;
        match expr.node(id) {
            Node::Target(pattern) => {
                let value = self.target(pattern, &vars).await;
                self.push(id, value);
            }
            Node::Set(patterns) => {
                let value = self.set(patterns, &vars).await;
                self.push(id, value);
            }
            Node::Let { name, value, body } => {
                if !is_variable_name(name) {
                    self.push(id, Err(QueryError::in_expression(format!("invalid variable name '{name}' in let expression"))));
                    return;
                }
                self.tasks.push(Task::Bind { id, name, body: *body, vars: vars.clone() });
                self.tasks.push(Task::Eval { id: *value, vars });
            }
            Node::Binary { op, operands } => {
                let Some(first) = operands.first() else {
                    self.push(id, Err(QueryError::in_evaluation("a set operation needs operands")));
                    return;
                };
                self.tasks.push(Task::Combine { id, op: *op, operands, index: 1, result: None, vars: vars.clone() });
                self.tasks.push(Task::Eval { id: *first, vars });
            }
            Node::Function { name, args } => {
                let values = args.iter().map(|_| None).collect();
                self.tasks.push(Task::Call { id, name, args, pending: None, values, vars });
            }
        }
    }

    async fn set(&mut self, patterns: &'a [String], vars: &Vars<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
        let mut nodes = NodeSet::new();
        for pattern in patterns {
            nodes.extend(self.target(pattern, vars).await?);
        }
        Ok(nodes)
    }

    async fn target(&mut self, pattern: &'a str, vars: &Vars<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
        let Some(name) = pattern.strip_prefix('$') else {
            if let Some(resolved) = self.patterns.get(pattern) {
                return resolved.clone();
            }
            let resolved = self.env.targets_matching(pattern).await;
            self.patterns.insert(pattern, resolved.clone());
            return resolved;
        };
        match lookup(vars, name) {
            Some(value) => Ok(value.clone()),
            None => Err(QueryError::in_expression(format!("undefined variable '{name}'"))),
        }
    }

    fn bind(&mut self, id: ExprId, name: &str, body: ExprId, vars: Vars<E::Node>) {
        let value = match self.pop() {
            Ok(value) => value,
            Err(error) => return self.push(id, Err(error)),
        };
        let vars = Some(Arc::new(Binding { name: name.to_string(), value, outer: vars }));
        self.tasks.push(Task::Attribute { id });
        self.tasks.push(Task::Eval { id: body, vars });
    }

    fn combine(&mut self, id: ExprId, op: SetOp, operands: &'a [ExprId], index: usize, result: Option<NodeSet<E::Node>>, vars: Vars<E::Node>) {
        let value = match self.pop() {
            Ok(value) => value,
            Err(error) => return self.push(id, Err(error)),
        };
        let result = match result {
            None => value,
            Some(result) => match op {
                SetOp::Union => result.union(value),
                SetOp::Intersect => result.intersection(&value),
                SetOp::Except => result.difference(&value),
            },
        };
        let Some(next) = operands.get(index) else {
            return self.values.push(Ok(result));
        };
        self.tasks.push(Task::Combine { id, op, operands, index: index + 1, result: Some(result), vars: vars.clone() });
        self.tasks.push(Task::Eval { id: *next, vars });
    }

    /// Evaluate the next of a call's argument expressions, or once they're
    /// evaluated, or one fails, call the function.
    async fn call(
        &mut self,
        id: ExprId,
        name: &'a str,
        args: &'a [Arg],
        pending: Option<usize>,
        mut values: Vec<Option<Result<NodeSet<E::Node>, QueryError>>>,
        vars: Vars<E::Node>,
    ) {
        let mut failed = false;
        let mut from = 0;
        if let Some(index) = pending {
            let value = self.pop();
            failed = value.is_err();
            values[index] = Some(value);
            from = index + 1;
        }
        let next = args.iter().enumerate().skip(from).find_map(|(index, arg)| match arg {
            Arg::Expr(expr) => Some((index, *expr)),
            _ => None,
        });
        if let (Some((index, expr)), false) = (next, failed) {
            self.tasks.push(Task::Call { id, name, args, pending: Some(index), values, vars: vars.clone() });
            self.tasks.push(Task::Eval { id: expr, vars });
            return;
        }
        let cx = Context { env: self.env, values: Mutex::new(values) };
        let value = functions::call(&cx, name, args).await;
        self.push(id, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{DEFAULT_FUNCTIONS, parse};

    /// A chain of targets, `//n:0` depending on `//n:1`, and so on to
    /// `//n:9`.
    struct Chain;

    #[async_trait]
    impl QueryEnvironment for Chain {
        type Node = u32;

        fn functions(&self) -> Vec<Function> {
            DEFAULT_FUNCTIONS.to_vec()
        }

        fn label(&self, node: &u32) -> Label {
            Label::parse_canonical(&format!("@@//n:{node}")).unwrap()
        }

        async fn targets_matching(&self, pattern: &str) -> Result<NodeSet<u32>, QueryError> {
            let node = pattern.strip_prefix("//n:").and_then(|name| name.parse().ok());
            node.map(|node| NodeSet::from_iter([node])).ok_or_else(|| QueryError::new(format!("no target {pattern}")))
        }

        async fn fwd_deps(&self, node: &u32) -> Result<Vec<u32>, QueryError> {
            Ok((*node < 9).then_some(node + 1).into_iter().collect())
        }

        async fn target_of(&self, _node: &u32) -> Result<TargetRef, QueryError> {
            Err(QueryError::new("no targets"))
        }

        async fn rule_class(&self, _class: &RuleClassRef) -> Result<Arc<RuleClass>, QueryError> {
            Err(QueryError::new("no rule classes"))
        }

        async fn attr_strings(&self, _node: &u32, _attr: Symbol) -> Result<Option<Vec<String>>, QueryError> {
            Ok(None)
        }

        async fn attr_nodes(&self, _node: &u32, _attr: Symbol) -> Result<Vec<u32>, QueryError> {
            Ok(Vec::new())
        }

        async fn siblings(&self, nodes: &NodeSet<u32>) -> Result<NodeSet<u32>, QueryError> {
            Ok(nodes.clone())
        }

        async fn is_visible(&self, _from: &u32, _target: &u32) -> Result<bool, QueryError> {
            Ok(true)
        }
    }

    fn evaluated(text: &str) -> Result<Vec<u32>, String> {
        let expr = parse(text, DEFAULT_FUNCTIONS).map_err(|error| error.0)?;
        let result = futures::executor::block_on(evaluate(&Chain, &expr)).map_err(|error| error.describe())?;
        Ok(result.into_vec())
    }

    #[test]
    fn evaluates_sets_lets_and_calls() {
        assert_eq!(evaluated("deps(//n:7)").unwrap(), [7, 8, 9]);
        assert_eq!(evaluated("deps(//n:7) - //n:8 + //n:1").unwrap(), [7, 9, 1]);
        assert_eq!(evaluated("let x = //n:3 in $x + deps(//n:8)").unwrap(), [3, 8, 9]);
        assert_eq!(evaluated("deps(//n:x)").unwrap_err(), "no target //n:x");
        assert_eq!(evaluated("let x = //n:3 in $y").unwrap_err(), "Evaluation of subquery \"$y\" failed (did you want to use --keep_going?): undefined variable 'y'");
    }

    #[test]
    fn evaluates_expressions_nested_arbitrarily_deeply() {
        const DEPTH: usize = 100_000;
        let calls = format!("{}//n:5{}", "deps(".repeat(DEPTH), ")".repeat(DEPTH));
        assert_eq!(evaluated(&calls).unwrap(), [5, 6, 7, 8, 9]);
        let lets: String = (0..DEPTH).map(|_| "let x = //n:1 in ").collect();
        assert_eq!(evaluated(&format!("{lets}$x")).unwrap(), [1]);
        let alternating: String = (0..DEPTH).map(|_| " + //n:2 - //n:2").collect();
        assert_eq!(evaluated(&format!("//n:1{alternating}")).unwrap(), [1]);
        let parens = format!("{}//n:4{}", "(".repeat(DEPTH), ")".repeat(DEPTH));
        assert_eq!(evaluated(&parens).unwrap(), [4]);
    }
}
