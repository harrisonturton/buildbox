//! `transition()`: configuration transitions, which change the options a
//! rule, or the dependencies through one of its attributes, are built with.

use crate::context::EvalContext;
use crate::convert::LabelParser;
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::SourcePosition;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::{Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless, ValueLike, starlark_value};
use std::fmt;

/// How transitions name native options: `//command_line_option:<name>`.
pub const COMMAND_LINE_OPTION_PREFIX: &str = "//command_line_option:";

/// A setting a transition reads or writes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Setting {
    /// A native option, by name.
    Native(String),
    /// A Starlark build setting, by label.
    Starlark(Label),
}

/// A transition: its implementation, which maps the settings it reads to
/// those it writes, given the rule's attributes.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct TransitionGen<V: ValueLifetimeless> {
    pub implementation: V,
    /// The settings it reads, each as the `.bzl` file names it and as
    /// resolved.
    #[freeze(identity)]
    #[allocative(skip)]
    pub inputs: Vec<(String, Setting)>,
    /// The settings it writes.
    #[freeze(identity)]
    #[allocative(skip)]
    pub outputs: Vec<(String, Setting)>,
    /// The `.bzl` file that called `transition()`, which labels it writes
    /// are relative to.
    #[freeze(identity)]
    #[allocative(skip)]
    pub definition: Label,
    /// Whether it's an exec transition, which starts from the native
    /// options' defaults rather than the configuration's.
    #[freeze(identity)]
    #[allocative(skip)]
    pub exec: bool,
    /// Where `transition()` was called, which errors in what it writes are
    /// reported at.
    #[freeze(identity)]
    #[allocative(skip)]
    pub location: Option<SourcePosition>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<TransitionGen<To>> for TransitionGen<From> {}

starlark_complex_value!(pub Transition);

impl<'v, V: ValueLike<'v>> fmt::Display for TransitionGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<transition object>")
    }
}

#[starlark_value(type = "transition")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TransitionGen<V> where Self: ProvidesStaticType<'v> {}

/// The setting `given` names: a native option, or a build setting's label
/// relative to the `.bzl` file.
fn setting(given: Value<'_>, parser: &LabelParser<'_>) -> anyhow::Result<(String, Setting)> {
    let text = match given.unpack_str() {
        Some(text) => text.to_string(),
        None => parser.label(given)?.to_string(),
    };
    let native = ["", "@", "@@"].iter().find_map(|repo| text.strip_prefix(repo).and_then(|rest| rest.strip_prefix(COMMAND_LINE_OPTION_PREFIX)));
    if let Some(name) = native {
        return Ok((text.clone(), Setting::Native(name.to_string())));
    }
    let label = parser.label(given)?;
    Ok((text, Setting::Starlark(label)))
}

/// The settings a list names, which mustn't repeat.
fn settings(list: Value<'_>, what: &str, parser: &LabelParser<'_>) -> anyhow::Result<Vec<(String, Setting)>> {
    let Some(items) = crate::convert::items(list) else {
        anyhow::bail!("transition(): {what} must be a list of strings, not {}", list.get_type());
    };
    let mut settings: Vec<(String, Setting)> = Vec::with_capacity(items.len());
    for item in items {
        let (given, setting) = setting(item, parser)?;
        if settings.iter().any(|(_, existing)| *existing == setting) {
            anyhow::bail!("duplicate {what} entry '{given}'");
        }
        settings.push((given, setting));
    }
    Ok(settings)
}

/// A transition `transition()` or `exec_transition()` makes.
fn new_transition<'v>(implementation: Value<'v>, inputs: Value<'v>, outputs: Value<'v>, exec: bool, eval: &Evaluator<'v, '_, '_>) -> anyhow::Result<Transition<'v>> {
    let context = EvalContext::get(eval)?;
    let definition = context.caller_file(eval);
    let mapping = context.mapping_for(definition.repo());
    let parser = LabelParser {
        package: definition.package,
        mapping: &*mapping,
    };
    Ok(TransitionGen {
        implementation,
        inputs: settings(inputs, "inputs", &parser)?,
        outputs: settings(outputs, "outputs", &parser)?,
        definition,
        exec,
        location: eval.call_stack_top_location().map(|span| crate::reported::call_position(&span)),
    })
}

#[starlark_module]
pub(crate) fn transition_function(builder: &mut GlobalsBuilder) {
    /// `transition(*, implementation, inputs, outputs)`.
    fn transition<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] inputs: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Transition<'v>> {
        new_transition(implementation, inputs, outputs, false, eval)
    }
}

#[starlark_module]
pub(crate) fn exec_transition_function(builder: &mut GlobalsBuilder) {
    /// `_builtins.toplevel.exec_transition(*, implementation, inputs,
    /// outputs)`: the transition `@_builtins` defines for the exec
    /// configuration.
    fn exec_transition<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] inputs: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Transition<'v>> {
        new_transition(implementation, inputs, outputs, true, eval)
    }
}

#[starlark_module]
fn py_internal_functions(builder: &mut GlobalsBuilder) {
    /// The name of the OS bb runs on, as Bazel names it.
    fn get_current_os_name() -> anyhow::Result<&'static str> {
        Ok(match std::env::consts::OS {
            "macos" => "osx",
            other => other,
        })
    }
}

/// Add `py_internal`, of which the exec transition reads the OS's name.
pub(crate) fn register_py_internal(builder: &mut GlobalsBuilder) {
    builder.namespace("py_internal", py_internal_functions);
}
