//! `--output=xml`: targets as Bazel's XML document, version 2.

use crate::engine::QueryError;
use crate::output::{OutputOptions, RepoPaths, display_label};
use crate::target_graph::possible_values;
use crate::universe::{TargetRef, Universe, attr_value};
use bazel_label::{Label, RepoMappingTable};
use bazel_packages::{AttrType, AttrValue, Target};
use skyframe::Compute;
use std::collections::BTreeSet;
use std::fmt::Write;

/// An XML element, which is written with its attributes sorted, as Java's
/// serializer writes them.
struct Element {
    name: &'static str,
    attributes: Vec<(&'static str, String)>,
    children: Vec<Element>,
}

impl Element {
    fn new(name: &'static str) -> Element {
        Element {
            name,
            attributes: Vec::new(),
            children: Vec::new(),
        }
    }

    fn with(mut self, attribute: &'static str, value: impl Into<String>) -> Element {
        self.attributes.push((attribute, value.into()));
        self
    }

    fn write(&self, depth: usize, out: &mut String) {
        let indent = "    ".repeat(depth);
        write!(out, "{indent}<{}", self.name).ok();
        let mut attributes = self.attributes.clone();
        attributes.sort();
        for (name, value) in attributes {
            write!(out, " {name}=\"{}\"", escape(&value)).ok();
        }
        if self.children.is_empty() {
            out.push_str("/>\n");
            return;
        }
        out.push_str(">\n");
        for child in &self.children {
            child.write(depth + 1, out);
        }
        writeln!(out, "{indent}</{}>", self.name).ok();
    }
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\n' => escaped.push_str("&#10;"),
            '\r' => escaped.push_str("&#13;"),
            '\t' => escaped.push_str("&#9;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Write the targets `labels` names as an XML document.
pub async fn write<C: Compute>(universe: &Universe<C>, labels: &[Label], paths: &RepoPaths<'_, C>, options: &OutputOptions, out: &mut String) -> Result<(), QueryError> {
    let mut query = Element::new("query").with("version", "2");
    for label in labels {
        let target = universe.target(*label).await?;
        let mut element = target_element(universe, &target, options).await?;
        element.attributes.push(("name", display_label(*label, &universe.main_mapping)));
        element.attributes.push(("location", paths.location(&target, options.relative_locations).await?));
        query.children.push(element);
    }
    out.push_str("<?xml version=\"1.1\" encoding=\"UTF-8\" standalone=\"no\"?>\n");
    query.write(0, out);
    Ok(())
}

async fn target_element<C: Compute>(universe: &Universe<C>, target: &TargetRef, options: &OutputOptions) -> Result<Element, QueryError> {
    let mapping = &universe.main_mapping;
    let package = target.package();
    match target.target() {
        Target::Rule(_) => rule_element(universe, target, options).await,
        Target::OutputFile { rule, .. } => Ok(Element::new("generated-file").with("generating-rule", display_label(package.label(*rule), mapping))),
        Target::SourceFile(file) => {
            let mut element = Element::new("source-file");
            if target.label() == package.build_file {
                let mut loads = BTreeSet::new();
                universe.loaded_files(package.loads.clone(), &mut loads).await?;
                element.children.extend(loads.into_iter().map(|load| Element::new("load").with("name", display_label(load, mapping))));
                element.children.extend(package.args.features.iter().map(|feature| Element::new("feature").with("name", feature.clone())));
                element.attributes.push(("package_contains_errors", (!package.errors.is_empty()).to_string()));
            }
            let visibility = file.visibility.clone().unwrap_or_else(|| vec![Label::parse_canonical("//visibility:private").expect("a valid label")]);
            for label in visibility {
                element.children.push(Element::new("visibility-label").with("name", display_label(label, mapping)));
            }
            Ok(element)
        }
        Target::PackageGroup(group) => {
            let mut element = Element::new("package-group");
            let mut includes = Element::new("list").with("name", "includes");
            includes.children.extend(group.includes.iter().map(|label| Element::new("label").with("value", display_label(*label, mapping))));
            let mut packages = Element::new("list").with("name", "packages");
            packages.children.extend(group.packages.iter().map(|spec| Element::new("string").with("value", spec.clone())));
            element.children.push(includes);
            element.children.push(packages);
            Ok(element)
        }
    }
}

async fn rule_element<C: Compute>(universe: &Universe<C>, target: &TargetRef, options: &OutputOptions) -> Result<Element, QueryError> {
    let Target::Rule(rule) = target.target() else {
        unreachable!("rule_element is given rules");
    };
    let mapping = &universe.main_mapping;
    let package = target.package();
    let class = universe.rule_class(package.rule_class(rule)).await?;
    let mut element = Element::new("rule").with("class", class.kind.to_string());
    let generated = crate::target_graph::generator_attrs(universe, target, rule).await?;
    for schema in &class.attrs {
        let given = schema.name.as_str() == "name" || rule.attr(schema.name).is_some();
        if given || options.xml_default_values {
            element.children.push(attribute_element(rule, schema, mapping));
        }
        if schema.name.as_str() == "tags" {
            for (name, value) in generated.iter().flatten() {
                element.children.push(string(value).with("name", *name));
            }
        }
    }
    if class.kind.as_str() == "test_suite" {
        if let Some(tests) = implicit_tests_element(target, mapping) {
            element.children.push(tests);
        }
    }
    for dep in universe.rule_inputs(target).await? {
        element.children.push(Element::new("rule-input").with("name", display_label(dep, mapping)));
    }
    for output in &rule.outputs {
        element.children.push(Element::new("rule-output").with("name", display_label(package.label(*output), mapping)));
    }
    for feature in &package.args.features {
        element.children.push(Element::new("rule-default-setting").with("name", feature.clone()));
    }
    Ok(element)
}

/// The element for a rule's attribute, as given or defaulted.
fn attribute_element(rule: &bazel_packages::Rule, schema: &bazel_packages::AttrSchema, mapping: &RepoMappingTable) -> Element {
    let value = match schema.name.as_str() {
        "name" => Some(AttrValue::String(rule.name.to_string())),
        _ => rule.attr_or_computed(schema.name).or_else(|| attr_value(rule, schema)).cloned(),
    };
    let mut element = value_element(schema.ty, value.as_ref(), mapping);
    element.attributes.push(("name", schema.name.to_string()));
    element
}

/// A test suite's `$implicit_tests`, which Bazel sets when it names no
/// tests.
fn implicit_tests_element(suite: &TargetRef, mapping: &RepoMappingTable) -> Option<Element> {
    let Target::Rule(rule) = suite.target() else {
        return None;
    };
    if matches!(rule.attr(intern::Symbol::intern("tests")), Some(AttrValue::LabelList(tests)) if !tests.is_empty()) {
        return None;
    }
    let tests = suite.package().implicit_tests(rule);
    let mut element = Element::new("list").with("name", "$implicit_tests");
    element.children.extend(tests.into_iter().map(|test| label(test, mapping)));
    Some(element)
}

/// The element for an attribute's value, or values, if it has a
/// `select()`: label lists list every label any branch has, other lists
/// every element of every branch, and other types say they're
/// configurable.
fn value_element(ty: AttrType, value: Option<&AttrValue>, mapping: &RepoMappingTable) -> Element {
    let values = match value {
        Some(value) => possible_values(value),
        None => Vec::new(),
    };
    if let Some(element) = list_element(ty, &values, mapping) {
        return element;
    }
    if let Some(element) = dict_element(ty, &values, mapping) {
        return element;
    }
    let mut element = Element::new(scalar_name(ty));
    match values.as_slice() {
        [] => {}
        [value] => {
            if let Some(text) = scalar_text(value, mapping) {
                element.attributes.push(("value", text));
            }
        }
        _ => element.attributes.push(("configurable", "yes".to_string())),
    }
    element
}

fn list_element(ty: AttrType, values: &[AttrValue], mapping: &RepoMappingTable) -> Option<Element> {
    let label_element = match ty {
        AttrType::OutputList => "output",
        _ => "label",
    };
    let mut element = Element::new("list");
    let mut seen_labels = BTreeSet::new();
    for value in values {
        match value {
            AttrValue::StringList(items) => element.children.extend(items.iter().map(|item| Element::new("string").with("value", item.clone()))),
            AttrValue::IntList(items) => element.children.extend(items.iter().map(|item| Element::new("int").with("value", item.to_string()))),
            AttrValue::LabelList(labels) => {
                for label in labels {
                    if values.len() == 1 || seen_labels.insert(*label) {
                        element.children.push(Element::new(label_element).with("value", display_label(*label, mapping)));
                    }
                }
            }
            _ => return None,
        }
    }
    Some(element)
}

fn dict_element(ty: AttrType, values: &[AttrValue], mapping: &RepoMappingTable) -> Option<Element> {
    let mut entries: Vec<(Element, Element)> = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let pairs: Vec<(String, Element, Element)> = match value {
            AttrValue::StringDict(entries) => entries.iter().map(|(k, v)| (k.clone(), string(k), string(v))).collect(),
            AttrValue::StringListDict(entries) => entries
                .iter()
                .map(|(k, v)| {
                    let mut list = Element::new("list");
                    list.children.extend(v.iter().map(|item| string(item)));
                    (k.clone(), string(k), list)
                })
                .collect(),
            AttrValue::LabelKeyedStringDict(entries) => entries.iter().map(|(k, v)| (k.to_string(), label(*k, mapping), string(v))).collect(),
            AttrValue::StringKeyedLabelDict(entries) => entries.iter().map(|(k, v)| (k.clone(), string(k), label(*v, mapping))).collect(),
            _ => return None,
        };
        for (key, key_element, value_element) in pairs {
            if seen.insert(key) {
                entries.push((key_element, value_element));
            }
        }
    }
    if values.is_empty() && !matches!(ty, AttrType::StringDict | AttrType::StringListDict | AttrType::LabelKeyedStringDict | AttrType::StringKeyedLabelDict) {
        return None;
    }
    let mut element = Element::new("dict");
    for (key, value) in entries {
        let mut pair = Element::new("pair");
        pair.children.push(key);
        pair.children.push(value);
        element.children.push(pair);
    }
    Some(element)
}

fn string(text: &str) -> Element {
    Element::new("string").with("value", text)
}

fn label(label: Label, mapping: &RepoMappingTable) -> Element {
    Element::new("label").with("value", display_label(label, mapping))
}

fn scalar_name(ty: AttrType) -> &'static str {
    match ty {
        AttrType::StringList | AttrType::LabelList | AttrType::OutputList | AttrType::IntList => "list",
        other => other.name(),
    }
}

fn scalar_text(value: &AttrValue, mapping: &RepoMappingTable) -> Option<String> {
    match value {
        AttrValue::None => None,
        AttrValue::Bool(b) => Some(b.to_string()),
        AttrValue::Int(i) => Some(i.to_string()),
        AttrValue::String(s) => Some(s.clone()),
        AttrValue::Label(label) => Some(display_label(*label, mapping)),
        other => Some(crate::target_graph::attr_string(other)),
    }
}
