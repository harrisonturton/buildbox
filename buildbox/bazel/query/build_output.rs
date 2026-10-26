//! `--output=build`: rules as they'd be written in a BUILD file, with the
//! calls that created them and defined their classes.

use crate::engine::QueryError;
use crate::output::{RepoPaths, display_label};
use crate::universe::{TargetRef, Universe};
use bazel_label::{Label, RepoMappingTable};
use bazel_packages::{AttrValue, CallFrame, Rule, SelectorPart, SourcePosition, Target};
use skyframe::Compute;
use std::fmt::Write;

/// The values a rule's attributes are printed with: as given, `select()`s
/// and all, for `query`; as configured, for `cquery`.
pub enum AttrValues<'a> {
    Given,
    Configured(&'a [(intern::Symbol, AttrValue)]),
}

/// Write a rule as `--output=build` prints it.
pub async fn write_rule<C: Compute>(universe: &Universe<C>, target: &TargetRef, paths: &RepoPaths<'_, C>, out: &mut String) -> Result<(), QueryError> {
    write_rule_with(universe, target, AttrValues::Given, paths, out).await
}

/// Write a rule as `--output=build` prints it, with its attributes'
/// `values`.
pub async fn write_rule_with<C: Compute>(
    universe: &Universe<C>,
    target: &TargetRef,
    values: AttrValues<'_>,
    paths: &RepoPaths<'_, C>,
    out: &mut String,
) -> Result<(), QueryError> {
    let Target::Rule(rule) = target.target() else {
        return Ok(());
    };
    let mapping = &universe.main_mapping;
    let class = universe.rule_class(target.package().rule_class(rule)).await?;
    let location = paths.location(target, false).await?;
    writeln!(out, "# {location}").ok();
    writeln!(out, "{}(", class.kind).ok();
    writeln!(out, "  name = \"{}\",", rule.name).ok();
    let generated = crate::target_graph::generator_attrs(universe, target, rule).await?;
    for schema in &class.attrs {
        if schema.name.as_str() == "name" {
            continue;
        }
        let value = match &values {
            AttrValues::Given => rule.attr(schema.name),
            AttrValues::Configured(configured) if rule.attr(schema.name).is_some() => configured.iter().find(|(name, _)| *name == schema.name).map(|(_, value)| value),
            AttrValues::Configured(_) => None,
        };
        if let Some(value) = value {
            writeln!(out, "  {} = {},", schema.name, build_value(value, mapping)).ok();
        }
        if schema.name.as_str() == "tags" {
            for (name, value) in generated.iter().flatten() {
                writeln!(out, "  {name} = {},", build_value(&AttrValue::String(value.clone()), mapping)).ok();
            }
        }
    }
    if class.kind.as_str() == "test_suite" && !names_tests(rule) {
        let tests = AttrValue::LabelList(target.package().implicit_tests(rule));
        writeln!(out, "  _implicit_tests = {},", build_value(&tests, mapping)).ok();
    }
    writeln!(out, ")").ok();
    let instantiated = instantiation_stack(target, rule);
    write_stack(&format!("# Rule {} instantiated at (most recent call last):", rule.name), &instantiated, paths, out).await;
    write_stack(&format!("# Rule {} defined at (most recent call last):", class.kind), &class.defined_at, paths, out).await;
    out.push('\n');
    Ok(())
}

/// Whether a test suite names its tests, rather than having its package's.
fn names_tests(suite: &Rule) -> bool {
    matches!(suite.attr(intern::Symbol::intern("tests")), Some(AttrValue::LabelList(tests)) if !tests.is_empty())
}

/// The calls that created a rule: those recorded for a macro's, or its
/// location in its BUILD file.
fn instantiation_stack(target: &TargetRef, rule: &Rule) -> Vec<CallFrame> {
    if !rule.call_stack.is_empty() {
        return rule.call_stack.clone();
    }
    let position = SourcePosition {
        file: target.package().build_file.to_string(),
        line: rule.location.line,
        column: rule.location.column,
    };
    vec![CallFrame {
        position,
        function: "<toplevel>".to_string(),
    }]
}

/// Write a call stack under `title`, each call's location padded to the
/// longest's, as Bazel writes them; nothing for an empty stack.
async fn write_stack<C: Compute>(title: &str, stack: &[CallFrame], paths: &RepoPaths<'_, C>, out: &mut String) {
    let mut locations = Vec::with_capacity(stack.len());
    for frame in stack {
        let file = paths.file_path(&frame.position.file).await;
        locations.push(format!("{file}:{}:{}", frame.position.line, frame.position.column));
    }
    let Some(width) = locations.iter().map(String::len).max() else {
        return;
    };
    writeln!(out, "{title}").ok();
    for (frame, location) in stack.iter().zip(&locations) {
        writeln!(out, "#   {location:width$} in {}", frame.function).ok();
    }
}

/// An attribute's value as Starlark source, with labels as `query` prints
/// them.
pub fn build_value(value: &AttrValue, mapping: &RepoMappingTable) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"));
    let label = |l: &Label| quote(&display_label(*l, mapping));
    let list = |items: Vec<String>| format!("[{}]", items.join(", "));
    let dict = |entries: Vec<(String, String)>| format!("{{{}}}", entries.into_iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", "));
    match value {
        AttrValue::None => "None".to_string(),
        AttrValue::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        AttrValue::Int(i) => i.to_string(),
        AttrValue::String(s) => quote(s),
        AttrValue::Label(l) => label(l),
        AttrValue::StringList(items) => list(items.iter().map(|s| quote(s)).collect()),
        AttrValue::LabelList(labels) => list(labels.iter().map(label).collect()),
        AttrValue::IntList(items) => list(items.iter().map(i64::to_string).collect()),
        AttrValue::StringDict(entries) => dict(entries.iter().map(|(k, v)| (quote(k), quote(v))).collect()),
        AttrValue::StringListDict(entries) => dict(entries.iter().map(|(k, v)| (quote(k), list(v.iter().map(|s| quote(s)).collect()))).collect()),
        AttrValue::LabelKeyedStringDict(entries) => dict(entries.iter().map(|(k, v)| (label(k), quote(v))).collect()),
        AttrValue::StringKeyedLabelDict(entries) => dict(entries.iter().map(|(k, v)| (quote(k), label(v))).collect()),
        AttrValue::Configurable(parts) => {
            let parts: Vec<String> = parts.iter().map(|part| selector_part(part, mapping)).collect();
            parts.join(" + ")
        }
    }
}

fn selector_part(part: &SelectorPart, mapping: &RepoMappingTable) -> String {
    match part {
        SelectorPart::Value(value) => build_value(value, mapping),
        SelectorPart::Select(select) => {
            let mut branches = Vec::with_capacity(select.branches.len());
            for (condition, value) in &select.branches {
                let condition = match condition.to_string() == bazel_packages::DEFAULT_CONDITION {
                    true => bazel_packages::DEFAULT_CONDITION.to_string(),
                    false => display_label(*condition, mapping),
                };
                branches.push(format!("\"{condition}\": {}", build_value(value, mapping)));
            }
            format!("select({{{}}})", branches.join(", "))
        }
    }
}
