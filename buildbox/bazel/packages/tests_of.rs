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

/// Whether a target passes `--build_tag_filters` or `--test_tag_filters`,
/// a comma-separated list of tags, those starting `-` excluded: a rule
/// passes if it has any of the other tags, or there are none, and none of
/// the excluded ones; other targets pass if no tags are required.
pub fn passes_tag_filters(rule: Option<&Rule>, filters: &str) -> bool {
    let mut required = Vec::new();
    let mut excluded = Vec::new();
    for filter in filters.split(',').filter(|filter| !filter.is_empty()) {
        match filter.strip_prefix('-') {
            Some(tag) => excluded.push(tag),
            None => required.push(filter.strip_prefix('+').unwrap_or(filter)),
        }
    }
    let Some(rule) = rule else {
        return required.is_empty();
    };
    let tags = string_list(rule, "tags");
    let has = |tag: &&str| tags.iter().any(|given| given == tag);
    !excluded.iter().any(has) && (required.is_empty() || required.iter().any(has))
}

/// The tests the test filters select, as Bazel's `TestFilter` does:
/// `--test_tag_filters`, `--test_size_filters`, `--test_timeout_filters`
/// and `--test_lang_filters`.
#[derive(Clone, Debug, Default)]
pub struct TestFilter {
    tags: String,
    /// The sizes allowed, if the sizes are filtered.
    sizes: Option<Vec<&'static str>>,
    /// The timeouts allowed, if the timeouts are filtered.
    timeouts: Option<Vec<&'static str>>,
    languages: Vec<String>,
    excluded_languages: Vec<String>,
}

const SIZES: &[&str] = &["small", "medium", "large", "enormous"];
const TIMEOUTS: &[&str] = &["short", "moderate", "long", "eternal"];

impl TestFilter {
    /// The filter the options' values give, or why they're invalid.
    pub fn new(tags: &str, sizes: &str, timeouts: &str, languages: &str) -> Result<TestFilter, String> {
        let mut filter = TestFilter {
            tags: tags.to_string(),
            sizes: allowed(sizes, SIZES, "test size").map_err(|error| format!("While parsing option --test_size_filters={sizes}: {error}"))?,
            timeouts: allowed(timeouts, TIMEOUTS, "test timeout").map_err(|error| format!("While parsing option --test_timeout_filters={timeouts}: {error}"))?,
            ..TestFilter::default()
        };
        for language in languages.split(',').filter(|language| !language.is_empty()) {
            match language.strip_prefix('-') {
                Some(excluded) => filter.excluded_languages.push(excluded.to_string()),
                None => filter.languages.push(language.to_string()),
            }
        }
        Ok(filter)
    }

    /// Whether a test, a rule of `kind`, passes the filters.
    pub fn passes(&self, rule: Option<&Rule>, kind: &str) -> bool {
        if !passes_tag_filters(rule, &self.tags) {
            return false;
        }
        let Some(rule) = rule else {
            return self.sizes.is_none() && self.timeouts.is_none();
        };
        let size = test_size(rule);
        if self.sizes.as_ref().is_some_and(|sizes| !sizes.contains(&size)) {
            return false;
        }
        let timeout = test_timeout(rule, size);
        if self.timeouts.as_ref().is_some_and(|timeouts| !timeouts.contains(&timeout)) {
            return false;
        }
        let language = rule_language(kind);
        let required = self.languages.is_empty() || self.languages.iter().any(|wanted| wanted == language);
        required && !self.excluded_languages.iter().any(|excluded| excluded == language)
    }
}

/// The values a filter of `kind` allows: those listed, less those listed
/// with `-`, or all but those, as Bazel's `EnumFilterConverter` reads it.
fn allowed(input: &str, values: &[&'static str], kind: &str) -> Result<Option<Vec<&'static str>>, String> {
    if input.is_empty() {
        return Ok(None);
    }
    let mut included = Vec::new();
    let mut excluded = Vec::new();
    for value in input.split(',') {
        let (list, name) = match value.strip_prefix('-') {
            Some(name) => (&mut excluded, name),
            None => (&mut included, value),
        };
        let Some(known) = values.iter().find(|known| known.eq_ignore_ascii_case(name)) else {
            return Err(format!("Invalid {kind} filter '{value}' in the input '{input}'"));
        };
        list.push(*known);
    }
    if included.is_empty() {
        included = values.to_vec();
    }
    included.retain(|value| !excluded.contains(value));
    if included.is_empty() {
        let mut name = kind.to_string();
        name[..1].make_ascii_uppercase();
        return Err(format!("{name} filter '{input}' definition cannot match any tests"));
    }
    Ok(Some(included))
}

/// A test's size, `medium` unless its `size` says otherwise.
fn test_size(rule: &Rule) -> &'static str {
    match rule.attr(Symbol::intern("size")) {
        Some(AttrValue::String(size)) => SIZES.iter().find(|known| **known == size).copied().unwrap_or("medium"),
        _ => "medium",
    }
}

/// A test's timeout: its `timeout`, or its size's.
fn test_timeout(rule: &Rule, size: &str) -> &'static str {
    if let Some(AttrValue::String(timeout)) = rule.attr(Symbol::intern("timeout")) {
        if let Some(known) = TIMEOUTS.iter().find(|known| **known == timeout) {
            return known;
        }
    }
    match size {
        "small" => "short",
        "large" => "long",
        "enormous" => "eternal",
        _ => "moderate",
    }
}

/// The language of a rule of `kind`: what comes before its last `_`, such
/// as `py` for `py_test`.
fn rule_language(kind: &str) -> &str {
    match kind.rsplit_once('_') {
        Some((language, _)) => language,
        None => kind,
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
