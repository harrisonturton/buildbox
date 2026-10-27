//! `testing`: the providers tests give, and analysis tests defined in
//! BUILD files.

use crate::context::EvalContext;
use crate::provider::{ProviderInstance, builtin_provider};
use crate::rule_value::{StarlarkRule, new_rule};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::none::NoneType;
use starlark::values::{StarlarkValue, Value};

/// The global of a BUILD file's module that holds the rule class
/// `testing.analysis_test` defined as `kind`: a name Starlark can't refer
/// to.
pub fn analysis_test_global(kind: &str) -> String {
    format!("analysis test {kind}")
}

#[starlark_module]
fn testing_functions(builder: &mut GlobalsBuilder) {
    /// `testing.TestEnvironment(environment, inherited_environment = [])`:
    /// a `RunEnvironmentInfo`, as Bazel gives it for this older name.
    fn TestEnvironment<'v>(
        environment: Value<'v>,
        #[starlark(default = NoneType)] inherited_environment: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let (provider, _) = builtin_provider("RunEnvironmentInfo").ok_or_else(|| anyhow::anyhow!("no RunEnvironmentInfo"))?;
        let heap = eval.heap();
        let inherited = match inherited_environment.is_none() {
            true => heap.alloc(starlark::values::list::AllocList::EMPTY),
            false => inherited_environment,
        };
        let fields = crate::fields::Fields::new(vec![("environment", environment), ("inherited_environment", inherited)]);
        Ok(heap.alloc(ProviderInstance {
            provider: provider.to_value(),
            fields,
        }))
    }

    /// `testing.analysis_test(name, implementation, attrs = {}, fragments =
    /// [], toolchains = [], attr_values = {})`: define an analysis test
    /// rule class in the BUILD file being evaluated, and its one target.
    fn analysis_test<'v>(
        name: &str,
        implementation: Value<'v>,
        #[starlark(require = named, default = NoneType)] attrs: Value<'v>,
        #[starlark(require = named, default = NoneType)] fragments: Value<'v>,
        #[starlark(require = named, default = NoneType)] toolchains: Value<'v>,
        #[starlark(require = named, default = NoneType)] attr_values: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        define_analysis_test(name, implementation, attrs, fragments, toolchains, attr_values, eval)?;
        Ok(NoneType)
    }
}

/// Define the rule class of `testing.analysis_test` and instantiate it.
/// Bazel names the class `<name>_test`, exported from the BUILD file.
fn define_analysis_test<'v>(
    name: &str,
    implementation: Value<'v>,
    attrs: Value<'v>,
    fragments: Value<'v>,
    toolchains: Value<'v>,
    attr_values: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<()> {
    let context = EvalContext::get(eval)?;
    if context.package.is_none() {
        anyhow::bail!("analysis_test can only be called in a BUILD thread");
    }
    if !is_identifier(name) {
        anyhow::bail!("'name' is limited to Starlark identifiers, got {name}");
    }
    let values = match attr_values.is_none() {
        true => Vec::new(),
        false => {
            let dict = DictRef::from_value(attr_values).ok_or_else(|| anyhow::anyhow!("attr_values must be a dict, not {}", attr_values.get_type()))?;
            let mut values = Vec::with_capacity(dict.len());
            for (key, value) in dict.iter() {
                let key = key.unpack_str().ok_or_else(|| anyhow::anyhow!("attr_values must have string keys"))?;
                values.push((key.to_string(), value));
            }
            values
        }
    };
    if values.iter().any(|(key, _)| key == "name") {
        anyhow::bail!("'name' cannot be set or overridden in 'attr_values'");
    }
    let mut arguments = vec![("analysis_test", Value::new_bool(true))];
    for (option, value) in [("attrs", attrs), ("fragments", fragments), ("toolchains", toolchains)] {
        if !value.is_none() {
            arguments.push((option, value));
        }
    }
    let rule = eval.heap().alloc(new_rule(implementation, arguments, eval)?);
    let kind = format!("{name}_test");
    let class = {
        let defined = StarlarkRule::from_value(rule).expect("new_rule makes a rule");
        defined.export_as(&kind, eval).map_err(|error| anyhow::anyhow!("Errors in exporting {name}: {error}"))?;
        defined.exported.get().cloned().expect("exported above")
    };
    eval.module().set(&analysis_test_global(&kind), rule);
    context.package_builder("analysis_test")?.defined_classes.push((*class).clone());
    let name_value = eval.heap().alloc(name);
    let mut named: Vec<(&str, Value<'v>)> = vec![("name", name_value)];
    named.extend(values.iter().map(|(key, value)| (key.as_str(), *value)));
    eval.eval_function(rule, &[], &named).map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(())
}

/// Whether `name` is a Starlark identifier, as analysis tests' names must
/// be.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    starts_well && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Add the `testing` namespace.
pub(crate) fn register(builder: &mut GlobalsBuilder) {
    builder.namespace("testing", |namespace| {
        testing_functions(namespace);
        crate::provider::register_namespace_providers(namespace, "testing");
    });
}
