//! `--output=starlark`: what a Starlark function, given each target,
//! returns.

use crate::configured_graph::ConfiguredGraph;
use crate::node::ConfiguredNode;
use allocative::Allocative;
use bazel_analysis::TargetValue;
use bazel_config::ConfigId;
use bazel_label::{Label, RepoMappingTable};
use bazel_query::{QueryError, display_label};
use bazel_starlark::label_value::LabelValue;
use skyframe::Compute;
use starlark::environment::{Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::AllocDict;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt::Write;

/// What each target is formatted with: `--starlark:expr`'s expression, or
/// `--starlark:file`'s `format` function.
pub enum Formatter {
    Expr(String),
    File(String),
}

/// The function a formatter defines, as Bazel's source for it.
fn program(formatter: &Formatter) -> (String, &'static str) {
    match formatter {
        Formatter::Expr(expr) if expr.is_empty() => ("def format(target): return (str(target.label))".to_string(), "invalid --starlark:expr: "),
        Formatter::Expr(expr) => (format!("def format(target): return ({expr})"), "invalid --starlark:expr: "),
        Formatter::File(source) => (source.clone(), "invalid --starlark:file: "),
    }
}

/// What the formatter returns for each node, as `str()` gives it, a line
/// each; a node it fails for is reported, and left out.
pub async fn format<C: Compute>(graph: &ConfiguredGraph<C>, nodes: &[ConfiguredNode], formatter: &Formatter) -> Result<(String, Vec<String>), QueryError> {
    let (source, prefix) = program(formatter);
    let ast = AstModule::parse("--starlark:expr", source, &Dialect::Extended).map_err(|error| QueryError::new(format!("{prefix}{error}")))?;
    let globals = GlobalsBuilder::standard().with(cquery_globals).build();
    let mut out = String::new();
    let mut errors = Vec::new();
    for node in nodes {
        let target = graph.configured(node).await;
        let providers = match &target {
            Some(target) => target.providers().ok(),
            None => None,
        };
        let input = Input {
            node: *node,
            providers: providers.as_ref().map(|providers| providers.owned()),
            mapping: &graph.packages.main_mapping,
        };
        match evaluate(&ast, &globals, &input) {
            Ok(Some(line)) => writeln!(out, "{line}").unwrap_or_default(),
            Ok(None) => {}
            Err(error) => errors.push(format!("Starlark evaluation error for {}: {error}", display_label(node.label, &graph.packages.main_mapping))),
        }
    }
    Ok((out, errors))
}

/// What the formatter is called with for one node.
struct Input<'a> {
    node: ConfiguredNode,
    providers: Option<&'a starlark::values::OwnedFrozenValue>,
    mapping: &'a RepoMappingTable,
}

/// Call the formatter's `format` with the node's target.
fn evaluate(ast: &AstModule, globals: &Globals, input: &Input<'_>) -> Result<Option<String>, String> {
    Module::with_temp_heap(|module| {
        let heap = module.heap();
        let mut eval = Evaluator::new(&module);
        eval.eval_module(ast.clone(), globals).map_err(|error| error.to_string())?;
        let format = module.get("format").ok_or_else(|| "file does not define 'format'".to_string())?;
        let target = target_value(input, heap);
        let result = eval.eval_function(format, &[target], &[]).map_err(|error| error.to_string())?;
        Ok(Some(result.to_str()))
    })
}

/// The value the formatter sees for a node: its configured target, whose
/// providers `providers()` gives, or for a source file, its label.
fn target_value<'v>(input: &Input<'_>, heap: Heap<'v>) -> Value<'v> {
    let Some(providers) = input.providers else {
        return heap.alloc(AllocStruct([("label", heap.alloc(LabelValue(input.node.label)))]));
    };
    let value = heap.access_owned_frozen_value(providers);
    let keys = provider_names(value, input.mapping);
    heap.alloc(QueryTarget {
        value,
        config: input.node.config,
        provider_names: keys,
    })
}

/// The names `providers()` gives a target's providers by: Starlark ones
/// by the label of the file defining them and their name.
fn provider_names(value: Value<'_>, mapping: &RepoMappingTable) -> Vec<String> {
    let Some(target) = TargetValue::from_value(value) else {
        return Vec::new();
    };
    let mut names = Vec::with_capacity(target.providers.len());
    for (key, _) in target.providers.iter() {
        let name = match key.split_once('%') {
            Some((file, name)) => match Label::parse_canonical(file) {
                Ok(file) => format!("{}%{name}", display_label(file, mapping)),
                Err(_) => key.clone(),
            },
            None => key.clone(),
        };
        names.push(name);
    }
    names
}

/// A configured target as `--output=starlark` sees it: the target its
/// dependents see, and its configuration.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct QueryTarget<'v> {
    #[allocative(skip)]
    value: Value<'v>,
    #[allocative(skip)]
    config: Option<ConfigId>,
    provider_names: Vec<String>,
}

impl std::fmt::Display for QueryTarget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.value)
    }
}

unsafe impl<'v> starlark::values::Trace<'v> for QueryTarget<'v> {
    fn trace(&mut self, tracer: &starlark::values::Tracer<'v>) {
        self.value.trace(tracer);
    }
}

#[starlark_value(type = "Target")]
impl<'v> StarlarkValue<'v> for QueryTarget<'v> {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.value.get_attr(attribute, heap).ok().flatten()
    }

    fn dir_attr(&self) -> Vec<String> {
        self.value.dir_attr()
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.value.at(index, heap)
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        self.value.is_in(other)
    }
}

impl<'v> starlark::values::AllocValue<'v> for QueryTarget<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex_no_freeze(self)
    }
}

#[starlark_module]
fn cquery_globals(builder: &mut GlobalsBuilder) {
    /// `providers(target)`: a target's providers, by name.
    fn providers<'v>(target: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let Some(query_target) = target.downcast_ref::<QueryTarget>() else {
            return Ok(Value::new_none());
        };
        let Some(value) = TargetValue::from_value(query_target.value) else {
            return Ok(Value::new_none());
        };
        let entries: Vec<(String, Value<'v>)> = query_target.provider_names.iter().cloned().zip(value.providers.values().map(|provider| provider.to_value())).collect();
        Ok(heap.alloc(AllocDict(entries)))
    }

    /// `build_options(target)`: the options a target is configured with,
    /// native ones under `//command_line_option:`.
    fn build_options<'v>(target: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let Some(config) = target.downcast_ref::<QueryTarget>().and_then(|target| target.config) else {
            return Ok(Value::new_none());
        };
        let config = config.get();
        let mut entries: Vec<(String, Value<'v>)> = Vec::new();
        for option in bazel_config::command_line_options() {
            if let Ok(value) = bazel_analysis::native_value(&config.options, option.name, heap) {
                entries.push((format!("//command_line_option:{}", option.name), value));
            }
        }
        for (label, value) in &config.options.starlark_flags {
            entries.push((label.to_string(), bazel_analysis::settings::starlark_value(value, heap)));
        }
        Ok(heap.alloc(AllocDict(entries)))
    }
}
