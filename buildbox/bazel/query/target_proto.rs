//! Targets as `build.proto`'s `Target` messages, which `query
//! --output=proto` prints and `cquery`'s proto output wraps.

use crate::build_output::AttrValues;
use crate::engine::QueryError;
use crate::output::{RepoPaths, display_label};
use crate::proto::Message;
use crate::target_graph::possible_values;
use crate::universe::{TargetRef, Universe, attr_value};
use bazel_label::{Label, RepoMappingTable};
use bazel_packages::{AttrSchema, AttrType, AttrValue, Target};
use skyframe::Compute;
use std::collections::BTreeSet;

/// A target as a `Target` message, its rule's attributes with `values`.
pub async fn target_message<C: Compute>(universe: &Universe<C>, target: &TargetRef, values: AttrValues<'_>, paths: &RepoPaths<'_, C>, relative: bool) -> Result<Message, QueryError> {
    let mapping = &universe.main_mapping;
    let package = target.package();
    let name = display_label(target.label(), mapping);
    let location = paths.location(target, relative).await?;
    let mut message = Message::new();
    match target.target() {
        Target::Rule(_) => {
            let rule = rule_message(universe, target, values, name, location).await?;
            message.enumeration(1, "type", 1, "RULE").message(2, "rule", rule);
        }
        Target::SourceFile(file) => {
            let mut source = Message::new();
            source.string(1, "name", name).string(2, "location", location);
            if target.label() == package.build_file {
                let mut loads = BTreeSet::new();
                universe.loaded_files(package.loads.clone(), &mut loads).await?;
                source.strings(3, "subinclude", loads.into_iter().map(|load| display_label(load, mapping)));
                source.strings(6, "feature", package.args.features.iter().cloned());
                source.bool(9, "package_contains_errors", !package.errors.is_empty());
            }
            let visibility = file.visibility.clone().unwrap_or_else(|| vec![Label::parse_canonical("@@//visibility:private").expect("a valid label")]);
            source.strings(5, "visibility_label", visibility.into_iter().map(|label| display_label(label, mapping)));
            message.enumeration(1, "type", 2, "SOURCE_FILE").message(3, "source_file", source);
        }
        Target::OutputFile { rule, .. } => {
            let mut generated = Message::new();
            generated.string(1, "name", name).string(2, "generating_rule", display_label(package.label(*rule), mapping)).string(3, "location", location);
            message.enumeration(1, "type", 3, "GENERATED_FILE").message(4, "generated_file", generated);
        }
        Target::PackageGroup(group) => {
            let mut package_group = Message::new();
            package_group.string(1, "name", name).strings(2, "contained_package", group.packages.iter().cloned());
            package_group.strings(3, "included_package_group", group.includes.iter().map(|label| display_label(*label, mapping)));
            message.enumeration(1, "type", 4, "PACKAGE_GROUP").message(5, "package_group", package_group);
        }
    }
    Ok(message)
}

async fn rule_message<C: Compute>(universe: &Universe<C>, target: &TargetRef, values: AttrValues<'_>, name: String, location: String) -> Result<Message, QueryError> {
    let Target::Rule(rule) = target.target() else {
        return Err(QueryError::in_evaluation(format!("{} isn't a rule", target.label())));
    };
    let mapping = &universe.main_mapping;
    let package = target.package();
    let class_ref = package.rule_class(rule);
    let class = universe.rule_class(class_ref).await?;
    let mut message = Message::new();
    message.string(1, "name", name).string(2, "rule_class", class.kind.to_string()).string(3, "location", location);
    let mut attributes: Vec<(String, Message)> = Vec::with_capacity(class.attrs.len() + 4);
    for schema in &class.attrs {
        let explicit = schema.name.as_str() == "name" || rule.attr(schema.name).is_some();
        let value = match (&values, schema.name.as_str()) {
            (_, "name") => Some(AttrValue::String(rule.name.to_string())),
            (_, "visibility") => {
                attributes.push((proto_name(schema), visibility_message(target, explicit, mapping)));
                continue;
            }
            (AttrValues::Configured(configured), _) => configured.iter().find(|(name, _)| *name == schema.name).map(|(_, value)| value.clone()),
            (AttrValues::Given, _) => rule.attr_or_computed(schema.name).or_else(|| attr_value(rule, schema)).cloned(),
        };
        attributes.push((proto_name(schema), attribute_message(schema, value.as_ref(), explicit, mapping)));
    }
    attributes.extend(synthesized_attributes(universe, target, rule, mapping).await?);
    attributes.extend(internal_attributes(&class, class_ref.definition.is_some()));
    attributes.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, attribute) in attributes {
        message.repeated(4, "attribute", attribute);
    }
    let inputs = universe.rule_inputs(target).await?;
    message.strings(5, "rule_input", inputs.into_iter().map(|input| display_label(input, mapping)));
    message.strings(6, "rule_output", rule.outputs.iter().map(|output| display_label(package.label(*output), mapping)));
    message.strings(7, "default_setting", package.args.features.iter().cloned());
    Ok(message)
}

/// The attributes Bazel gives every rule that bb keeps elsewhere: the
/// macro that created it, and the conditions of its `select()`s.
async fn synthesized_attributes<C: Compute>(universe: &Universe<C>, target: &TargetRef, rule: &bazel_packages::Rule, mapping: &RepoMappingTable) -> Result<Vec<(String, Message)>, QueryError> {
    let generated = crate::target_graph::generator_attrs(universe, target, rule).await?;
    let generator = |name: &str| generated.iter().flatten().find(|(attr, _)| *attr == name).map(|(_, value)| value.clone()).unwrap_or_default();
    let mut conditions = Vec::new();
    for (_, value) in &rule.attrs {
        value.conditions(&mut conditions);
    }
    conditions.retain(|condition| condition.to_string() != bazel_packages::DEFAULT_CONDITION);
    conditions.sort();
    conditions.dedup();
    let string = |name: &str, value: String| {
        let mut message = Message::new();
        message.string(1, "name", name.to_string()).enumeration(2, "type", 2, "STRING").string(5, "string_value", value).bool(13, "explicitly_specified", false).bool(20, "nodep", false);
        (name.to_string(), message)
    };
    let mut config_dependencies = Message::new();
    config_dependencies.string(1, "name", "$config_dependencies").enumeration(2, "type", 6, "LABEL_LIST");
    config_dependencies.strings(6, "string_list_value", conditions.iter().map(|label| display_label(*label, mapping)));
    config_dependencies.bool(13, "explicitly_specified", false).bool(20, "nodep", false);
    Ok(vec![
        string("generator_function", generator("generator_function")),
        string("generator_location", generator("generator_location")),
        string("generator_name", generator("generator_name")),
        ("$config_dependencies".to_string(), config_dependencies),
    ])
}

/// The rule classes Bazel defines without its base rule class's
/// attributes, such as `:action_listener`.
const WITHOUT_BASE_ATTRIBUTES: &[&str] = &["alias", "constraint_setting", "constraint_value", "label_flag", "label_setting", "platform", "toolchain_type"];

/// The internal attributes Bazel gives a rule's class, which bb has no use
/// for: whether a Starlark rule is executable, the extra action listeners
/// most rules have, and `config_setting`'s flag aliases.
fn internal_attributes(class: &bazel_packages::RuleClass, is_starlark: bool) -> Vec<(String, Message)> {
    let mut attributes = Vec::new();
    if is_starlark && (class.executable || class.test) {
        let mut message = Message::new();
        message.string(1, "name", "$is_executable").enumeration(2, "type", 14, "BOOLEAN");
        message.int(3, "int_value", 1).string(5, "string_value", "true").bool(13, "explicitly_specified", false).bool(14, "boolean_value", true);
        attributes.push(("$is_executable".to_string(), message));
    }
    let empty_label_list = |name: &str| {
        let mut message = Message::new();
        message.string(1, "name", name.to_string()).enumeration(2, "type", 6, "LABEL_LIST").bool(13, "explicitly_specified", false).bool(20, "nodep", false);
        (name.to_string(), message)
    };
    if !WITHOUT_BASE_ATTRIBUTES.contains(&class.kind.as_str()) {
        attributes.push(empty_label_list(":action_listener"));
    }
    if class.kind.as_str() == "config_setting" {
        attributes.push(empty_label_list(":flag_alias_settings"));
    }
    attributes
}

/// A rule's `visibility`, which Bazel's protos give as the labels of its
/// visibility, as strings.
fn visibility_message(target: &TargetRef, explicit: bool, mapping: &RepoMappingTable) -> Message {
    let labels = crate::target_graph::visibility_of(target);
    let labels = match labels.is_empty() {
        true => vec![Label::parse_canonical("@@//visibility:private").expect("a valid label")],
        false => labels,
    };
    let mut message = Message::new();
    message.string(1, "name", "visibility").enumeration(2, "type", 5, "STRING_LIST");
    message.strings(6, "string_list_value", labels.into_iter().map(|label| display_label(label, mapping)));
    message.bool(13, "explicitly_specified", explicit).bool(20, "nodep", true);
    message
}

/// An attribute's name as Bazel's protos give it: a Starlark rule's
/// private attributes start with `$`, or `:` if the configuration chooses
/// their default.
fn proto_name(schema: &AttrSchema) -> String {
    let Some(rest) = schema.name.as_str().strip_prefix('_') else {
        return schema.name.to_string();
    };
    match schema.default {
        bazel_packages::AttrDefault::LateBound(_) => format!(":{rest}"),
        _ => format!("${rest}"),
    }
}

/// An `Attribute` message: its value, flattened if it has a `select()`,
/// as `--proto:flatten_selects` does by default.
fn attribute_message(schema: &AttrSchema, value: Option<&AttrValue>, explicit: bool, mapping: &RepoMappingTable) -> Message {
    let (type_number, type_name) = discriminator(schema.ty);
    let mut message = Message::new();
    message.string(1, "name", proto_name(schema)).enumeration(2, "type", type_number, type_name).bool(13, "explicitly_specified", explicit);
    if is_dependency_type(schema.ty) {
        message.bool(20, "nodep", false);
    }
    let Some(value) = value else {
        return message;
    };
    let flattened = flattened(value);
    match (schema.ty, &flattened) {
        (AttrType::Tristate, AttrValue::Int(value)) => write_tristate(&mut message, *value),
        _ => write_value(&mut message, &flattened, mapping),
    }
    message
}

/// A tristate, as Bazel encodes it: as an int, a string and the enum.
fn write_tristate(message: &mut Message, value: i64) {
    let (name, number, text) = match value {
        1 => ("YES", 1, "yes"),
        0 => ("NO", 0, "no"),
        _ => ("AUTO", 2, "auto"),
    };
    message.int(3, "int_value", value).string(5, "string_value", text).enumeration(15, "tristate_value", number, name);
}

/// A value with every branch of its `select()`s: the union of label
/// lists' labels, or the only value a scalar can take.
fn flattened(value: &AttrValue) -> AttrValue {
    let mut values = possible_values(value);
    if values.len() == 1 {
        return values.remove(0);
    }
    match values.first() {
        Some(AttrValue::LabelList(_)) => {
            let mut labels = Vec::new();
            for value in &values {
                if let AttrValue::LabelList(list) = value {
                    labels.extend(list.iter().copied().filter(|label| !labels.contains(label)).collect::<Vec<_>>());
                }
            }
            AttrValue::LabelList(labels)
        }
        Some(first) => first.clone(),
        None => AttrValue::None,
    }
}

fn write_value(message: &mut Message, value: &AttrValue, mapping: &RepoMappingTable) {
    let label = |label: &Label| display_label(*label, mapping);
    match value {
        AttrValue::None | AttrValue::Configurable(_) => {}
        AttrValue::Bool(value) => {
            message.int(3, "int_value", i64::from(*value)).string(5, "string_value", value.to_string()).bool(14, "boolean_value", *value);
        }
        AttrValue::Int(value) => {
            message.int(3, "int_value", *value);
        }
        AttrValue::String(text) => {
            message.string(5, "string_value", text.clone());
        }
        AttrValue::Label(value) => {
            message.string(5, "string_value", label(value));
        }
        AttrValue::StringList(items) => {
            message.strings(6, "string_list_value", items.iter().cloned());
        }
        AttrValue::LabelList(labels) => {
            message.strings(6, "string_list_value", labels.iter().map(label));
        }
        AttrValue::IntList(items) => {
            message.ints(17, "int_list_value", items.iter().copied());
        }
        AttrValue::StringDict(entries) => {
            for (key, value) in entries {
                let mut entry = Message::new();
                entry.string(1, "key", key.clone()).string(2, "value", value.clone());
                message.repeated(8, "string_dict_value", entry);
            }
        }
        AttrValue::StringListDict(entries) => {
            for (key, values) in entries {
                let mut entry = Message::new();
                entry.string(1, "key", key.clone()).strings(2, "value", values.iter().cloned());
                message.repeated(11, "string_list_dict_value", entry);
            }
        }
        AttrValue::LabelKeyedStringDict(entries) => {
            for (key, value) in entries {
                let mut entry = Message::new();
                entry.string(1, "key", label(key)).string(2, "value", value.clone());
                message.repeated(22, "label_keyed_string_dict_value", entry);
            }
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            for (key, value) in entries {
                let mut entry = Message::new();
                entry.string(1, "key", key.clone()).string(2, "value", label(value));
                message.repeated(19, "label_dict_unary_value", entry);
            }
        }
    }
}

/// Whether Bazel says if an attribute of this type is a dependency.
fn is_dependency_type(ty: AttrType) -> bool {
    matches!(
        ty,
        AttrType::String | AttrType::Label | AttrType::Output | AttrType::StringList | AttrType::LabelList | AttrType::StringKeyedLabelDict | AttrType::LabelKeyedStringDict | AttrType::OutputList
    )
}

/// The `Attribute.Discriminator` of a type: its number and name.
fn discriminator(ty: AttrType) -> (i32, &'static str) {
    match ty {
        AttrType::Int => (1, "INTEGER"),
        AttrType::String => (2, "STRING"),
        AttrType::Label => (3, "LABEL"),
        AttrType::Output => (4, "OUTPUT"),
        AttrType::StringList => (5, "STRING_LIST"),
        AttrType::LabelList => (6, "LABEL_LIST"),
        AttrType::OutputList => (7, "OUTPUT_LIST"),
        AttrType::License => (9, "LICENSE"),
        AttrType::StringDict => (10, "STRING_DICT"),
        AttrType::StringListDict => (13, "STRING_LIST_DICT"),
        AttrType::Bool => (14, "BOOLEAN"),
        AttrType::Tristate => (15, "TRISTATE"),
        AttrType::IntList => (16, "INTEGER_LIST"),
        AttrType::StringKeyedLabelDict => (19, "LABEL_DICT_UNARY"),
        AttrType::LabelKeyedStringDict => (21, "LABEL_KEYED_STRING_DICT"),
    }
}
