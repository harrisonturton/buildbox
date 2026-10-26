//! Creating a rule in the package being defined.

use crate::context::EvalContext;
use crate::convert::{self, LabelParser};
use crate::select::selector_parts;
use crate::reported::call_position;
use bazel_packages::{AttrType, AttrValue, Generator, Location, PackageError, Rule, RuleClass, RuleClassRef};
use intern::Symbol;
use starlark::codemap::FileSpan;
use starlark::eval::Evaluator;
use starlark::values::Value;

/// Create a rule of class `schema` from the keyword arguments of a call,
/// and the values computed for attributes with computed defaults. A rule
/// that isn't valid, such as one with an attribute its class doesn't have,
/// is an error of the package, at the call in the BUILD file, and the BUILD
/// file's evaluation goes on, as Bazel's does; the rule is still created,
/// without the attributes in error.
pub fn instantiate<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    class: RuleClassRef,
    schema: &RuleClass,
    kwargs: &[(&str, Value<'v>)],
    computed: &[(&str, Value<'v>)],
) -> anyhow::Result<()> {
    let context = EvalContext::get(eval)?;
    let (location, generator) = call_site(eval);
    let call_stack = match generator {
        Some(_) => crate::reported::call_frames(eval),
        None => Vec::new(),
    };
    let position = eval.call_stack().frames.first().and_then(|frame| frame.location.as_ref()).map(call_position);
    let mut builder = context.package_builder(&format!("{}()", schema.kind))?;
    let package = builder.id;
    let mapping = context.mapping_for(package.repo);
    let parser = LabelParser {
        package,
        mapping: &*mapping,
    };

    let name = rule_name(schema, kwargs)?;
    let (mut rule, mut errors) = rule(schema, kwargs, computed, &parser, name, location, generator);
    rule.call_stack = call_stack;
    if let Err(error) = builder.add_rule(class, schema, rule) {
        errors.push(error);
    }
    for error in errors {
        let error = match &position {
            Some(position) => PackageError::at(position.clone(), error.to_string()),
            None => PackageError::new(error.to_string()),
        };
        builder.errors.push(error);
    }
    Ok(())
}

/// The rule `kwargs` and `computed` make, named `name`, and the errors in
/// the attributes it was given, which it's made without.
fn rule<'v>(
    schema: &RuleClass,
    kwargs: &[(&str, Value<'v>)],
    computed: &[(&str, Value<'v>)],
    parser: &LabelParser<'_>,
    name: Symbol,
    location: Location,
    generator: Option<Generator>,
) -> (Rule, Vec<anyhow::Error>) {
    let package = parser.package;
    let mut attrs = Vec::with_capacity(kwargs.len());
    let mut errors = Vec::new();
    for (attr, value) in kwargs {
        if *attr == "name" || value.is_none() {
            continue;
        }
        match attribute(schema, attr, *value, parser, name) {
            Ok(converted) => attrs.push(converted),
            Err(error) => errors.push(error),
        }
    }
    if let Err(error) = check_mandatory(schema, &attrs, name, &package) {
        errors.push(error);
    }

    let mut computed_attrs = Vec::with_capacity(computed.len());
    for (attr, value) in computed {
        let symbol = Symbol::intern(attr);
        let Some(attr_schema) = schema.attr(symbol) else {
            continue;
        };
        if value.is_none() {
            continue;
        }
        if let Ok(converted) = convert::attr_value(*value, attr_schema.ty, parser, attr) {
            computed_attrs.push((symbol, converted));
        }
    }

    let outputs = outputs(schema, &attrs, name);
    let rule = Rule {
        name,
        class: 0,
        attrs,
        location,
        generator,
        outputs,
        computed: computed_attrs,
        call_stack: Vec::new(),
    };
    (rule, errors)
}

/// The attribute `attr` of the rule `name` given `value`.
fn attribute(schema: &RuleClass, attr: &str, value: Value<'_>, parser: &LabelParser<'_>, name: Symbol) -> anyhow::Result<(Symbol, AttrValue)> {
    let label = parser.package.label(name);
    let symbol = Symbol::intern(attr);
    let Some(attr_schema) = schema.attr(symbol) else {
        anyhow::bail!("{label}: no such attribute '{attr}' in '{}' rule", schema.kind);
    };
    if !attr_schema.configurable && selector_parts(value).is_some() {
        anyhow::bail!("{label}: attribute '{attr}' is not configurable");
    }
    let converted = convert::attr_value(value, attr_schema.ty, parser, attr).map_err(|error| anyhow::anyhow!("{label}: {error}"))?;
    Ok((symbol, converted))
}

fn rule_name(schema: &RuleClass, kwargs: &[(&str, Value<'_>)]) -> anyhow::Result<Symbol> {
    let Some((_, name)) = kwargs.iter().find(|(attr, _)| *attr == "name") else {
        anyhow::bail!("{}() missing mandatory argument 'name'", schema.kind);
    };
    let Some(name) = name.unpack_str() else {
        anyhow::bail!("{}(): 'name' must be a string, not {}", schema.kind, name.get_type());
    };
    convert::target_name(name)
}

fn check_mandatory(schema: &RuleClass, attrs: &[(Symbol, AttrValue)], name: Symbol, package: &bazel_label::PackageId) -> anyhow::Result<()> {
    for attr in &schema.attrs {
        if !attr.mandatory || attr.name.as_str() == "name" {
            continue;
        }
        if !attrs.iter().any(|(given, _)| *given == attr.name) {
            anyhow::bail!("{}: missing value for mandatory attribute '{}' in '{}' rule", package.label(name), attr.name, schema.kind);
        }
    }
    Ok(())
}

/// The files a rule declares: the values of its output attributes, and the
/// class's predeclared outputs.
fn outputs(schema: &RuleClass, attrs: &[(Symbol, AttrValue)], name: Symbol) -> Vec<Symbol> {
    let mut outputs = Vec::new();
    for (attr, value) in attrs {
        let is_output = schema.attr(*attr).is_some_and(|attr| attr.ty.is_output());
        if !is_output {
            continue;
        }
        let mut labels = Vec::new();
        match value {
            AttrValue::Label(label) => labels.push(*label),
            AttrValue::LabelList(list) => labels.extend(list),
            _ => {}
        }
        outputs.extend(labels.into_iter().map(|label| label.name));
    }
    for (_, template) in &schema.outputs {
        outputs.push(Symbol::intern(&template.replace("%{name}", name.as_str())));
    }
    outputs
}

/// Where the BUILD file called the rule, directly or through macros, and
/// the outermost macro if there was one.
fn call_site(eval: &Evaluator<'_, '_, '_>) -> (Location, Option<Generator>) {
    let stack = eval.call_stack();
    let Some(outermost) = stack.frames.first() else {
        return (Location::default(), None);
    };
    let location = outermost.location.as_ref().map(location_of).unwrap_or_default();
    if stack.frames.len() < 2 {
        return (location, None);
    }
    let name = outermost.location.as_ref().and_then(|span| literal_name(span.source_span()));
    let generator = Generator {
        function: Symbol::intern(&outermost.name),
        name: name.map(|name| Symbol::intern(&name)).unwrap_or(Symbol::EMPTY),
        location,
    };
    (location, Some(generator))
}

/// The `name = "..."` argument of a call, if it's given as a literal at
/// the call's top level, as macros' names almost always are.
fn literal_name(call: &str) -> Option<String> {
    let open = call.find('(')?;
    let mut depth = 0;
    let bytes = call.as_bytes();
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' | b'\'' => {
                let quote = bytes[index];
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    index += if bytes[index] == b'\\' { 2 } else { 1 };
                }
            }
            b'n' if depth == 0 && call[index..].starts_with("name") && !is_ident(bytes.get(index.wrapping_sub(1))) => {
                let rest = call[index + 4..].trim_start();
                if let Some(value) = rest.strip_prefix('=') {
                    let value = value.trim_start();
                    let quote = value.chars().next()?;
                    if quote == '"' || quote == '\'' {
                        let end = value[1..].find(quote)?;
                        return Some(value[1..1 + end].to_string());
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn is_ident(byte: Option<&u8>) -> bool {
    byte.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Where a call is, as Bazel reports it: at the call's opening
/// parenthesis.
fn location_of(span: &FileSpan) -> Location {
    let resolved = span.resolve_span();
    let text = span.source_span();
    let first_line = text.lines().next().unwrap_or("");
    let paren = first_line.find('(').map(|offset| first_line[..offset].chars().count()).unwrap_or(0);
    Location {
        line: resolved.begin.line as u32 + 1,
        column: (resolved.begin.column + paren) as u32 + 1,
    }
}

/// Whether an attribute type is a list that a rule may give as a select.
pub fn is_list(ty: AttrType) -> bool {
    matches!(ty, AttrType::StringList | AttrType::LabelList | AttrType::IntList | AttrType::OutputList)
}
