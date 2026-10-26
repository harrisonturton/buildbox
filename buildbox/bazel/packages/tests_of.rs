//! Test rules and the tests a `test_suite` names, as Bazel tells them
//! from a package alone.

use crate::{AttrValue, Package, Rule};
use bazel_label::Label;
use intern::Symbol;

/// Which tests a test suite's tags select: those with every required tag
/// and none excluded, a tag starting with `-` excluding.
pub struct TagFilter {
    required: Vec<String>,
    excluded: Vec<String>,
}

impl TagFilter {
    /// The filter a test suite's `tags` give: `manual` selects nothing.
    pub fn of(tags: &[String]) -> TagFilter {
        let mut filter = TagFilter {
            required: Vec::new(),
            excluded: Vec::new(),
        };
        for tag in tags {
            if let Some(tag) = tag.strip_prefix('-') {
                filter.excluded.push(tag.to_string());
            } else if let Some(tag) = tag.strip_prefix('+') {
                filter.required.push(tag.to_string());
            } else if tag != "manual" {
                filter.required.push(tag.clone());
            }
        }
        filter
    }

    /// Whether the filter selects a test, by its tags and its size.
    pub fn includes(&self, test: &Rule) -> bool {
        let mut tags = string_list(test, "tags");
        let size = match test.attr(Symbol::intern("size")) {
            Some(AttrValue::String(size)) => size.clone(),
            _ => "medium".to_string(),
        };
        tags.push(size);
        let excluded = self.excluded.iter().any(|tag| tags.contains(tag));
        let required = self.required.iter().all(|tag| tags.contains(tag));
        !excluded && required
    }
}

/// A rule's string list attribute, as given.
fn string_list(rule: &Rule, name: &str) -> Vec<String> {
    match rule.attr(Symbol::intern(name)) {
        Some(AttrValue::StringList(values)) => values.clone(),
        _ => Vec::new(),
    }
}

/// Whether a rule of this kind is a test, as Bazel tells: its kind ends
/// `_test`.
pub fn is_test_kind(kind: &str) -> bool {
    kind.ends_with("_test")
}

/// Whether a rule is tagged `manual`, which wildcards and test suites skip.
pub fn is_manual(rule: &Rule) -> bool {
    string_list(rule, "tags").iter().any(|tag| tag == "manual")
}

impl Package {
    /// A test suite's `$implicit_tests`: when it names no tests, those of
    /// its package that aren't tagged `manual`, which its tags select, in
    /// label order.
    pub fn implicit_tests(&self, suite: &Rule) -> Vec<Label> {
        if let Some(AttrValue::LabelList(tests)) = suite.attr(Symbol::intern("tests")) {
            if !tests.is_empty() {
                return Vec::new();
            }
        }
        let filter = TagFilter::of(&string_list(suite, "tags"));
        let mut tests: Vec<Label> = self
            .rules()
            .filter(|rule| is_test_kind(self.rule_class(rule).kind.as_str()) && !is_manual(rule) && filter.includes(rule))
            .map(|rule| self.label(rule.name))
            .collect();
        tests.sort();
        tests
    }
}
