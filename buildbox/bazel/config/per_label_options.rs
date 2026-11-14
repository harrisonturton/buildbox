//! `--per_file_copt`'s values, as Bazel's `PerLabelOptions` and
//! `RegexFilter` read and match them: `filter@option,option`, where the
//! filter is a list of Java regular expressions saying which files get the
//! options.

use bazel_label::Label;
use fancy_regex::Regex;

/// A value that isn't a filter, or a filter that can't be matched.
#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("Failed to build filter: value looks like another flag ({0}). Either escape the value with \"\\-\\-\", or pass an explicit value to the flag.")]
    LooksLikeFlag(String),

    #[error("Failed to build valid regular expression: {0}")]
    Invalid(Box<fancy_regex::Error>),

    #[error("matching '{value}' against '{regex}': {error}")]
    Match {
        value: String,
        regex: String,
        error: Box<fancy_regex::Error>,
    },
}

/// Regular expressions that take some strings and leave out others, as
/// Bazel's `RegexFilter`: a string passes when no exclusion matches part of
/// it and, if there are inclusions, one of them does.
#[derive(Debug, Clone)]
pub struct RegexFilter {
    inclusions: Vec<Regex>,
    exclusions: Vec<Regex>,
}

impl RegexFilter {
    /// A filter from its text: regular expressions separated by commas,
    /// each an exclusion if it starts with `-`. `\,` is a comma within one,
    /// and empty ones are left out.
    pub fn parse(text: &str) -> Result<RegexFilter, FilterError> {
        if text.starts_with("--") {
            return Err(FilterError::LooksLikeFlag(text.to_string()));
        }
        let mut filter = RegexFilter {
            inclusions: Vec::new(),
            exclusions: Vec::new(),
        };
        for piece in split_at_commas(text) {
            match piece.strip_prefix('-') {
                Some("") => {}
                Some(exclusion) => filter.exclusions.push(compile(exclusion)?),
                None => filter.inclusions.push(compile(&piece)?),
            }
        }
        Ok(filter)
    }

    /// Whether `value` passes the filter. The expressions match anywhere in
    /// it, as Java's `Matcher.find()` does, so `^` and `$` anchor them.
    pub fn is_included(&self, value: &str) -> Result<bool, FilterError> {
        if any_matches(&self.exclusions, value)? {
            return Ok(false);
        }
        if self.inclusions.is_empty() {
            return Ok(true);
        }
        any_matches(&self.inclusions, value)
    }
}

/// Options for the files a filter takes: one value of `--per_file_copt`.
#[derive(Debug, Clone)]
pub struct PerLabelOptions {
    pub filter: RegexFilter,
    pub options: Vec<String>,
}

impl PerLabelOptions {
    /// A value from its text, `filter@option,option`. The filter ends at the
    /// first `@`, so an `@` in it is written `\x40`. `\,` is a comma within
    /// an option, and empty options are left out. Without an `@`, the text
    /// is all filter and there are no options.
    pub fn parse(text: &str) -> Result<PerLabelOptions, FilterError> {
        let (filter, options) = text.split_once('@').unwrap_or((text, ""));
        Ok(PerLabelOptions {
            filter: RegexFilter::parse(filter)?,
            options: split_at_commas(options),
        })
    }

    /// Whether a source file gets the options: when the filter takes its
    /// label, as Bazel prints one (`//pkg:name`, or `@@repo//pkg:name` in
    /// another repository), or its path in the execution root.
    pub fn applies_to(&self, label: Option<Label>, exec_path: &str) -> Result<bool, FilterError> {
        let by_label = match label {
            Some(label) => self.filter.is_included(&label.to_string())?,
            None => false,
        };
        Ok(by_label || self.filter.is_included(exec_path)?)
    }
}

fn compile(regex: &str) -> Result<Regex, FilterError> {
    Regex::new(regex).map_err(|error| FilterError::Invalid(Box::new(error)))
}

fn any_matches(regexes: &[Regex], value: &str) -> Result<bool, FilterError> {
    for regex in regexes {
        let found = regex.is_match(value).map_err(|error| FilterError::Match {
            value: value.to_string(),
            regex: regex.as_str().to_string(),
            error: Box::new(error),
        })?;
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `text` split at each comma that doesn't follow a backslash, with `\,`
/// read as a comma, leaving out empty parts.
fn split_at_commas(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut previous = None;
    for c in text.chars() {
        match (c, previous) {
            (',', Some('\\')) => {
                part.pop();
                part.push(',');
            }
            (',', _) => parts.push(std::mem::take(&mut part)),
            _ => part.push(c),
        }
        previous = Some(c);
    }
    parts.push(part);
    parts.retain(|part| !part.is_empty());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(text: &str) -> Option<Label> {
        Some(Label::parse_canonical(text).unwrap())
    }

    fn applies(value: &str, label_text: &str, exec_path: &str) -> bool {
        PerLabelOptions::parse(value).unwrap().applies_to(label(label_text), exec_path).unwrap()
    }

    const LLVM_LABEL: &str = "@@+llvm_project+llvm-project//llvm:lib/Demangle/Demangle.cpp";
    const LLVM_PATH: &str = "external/+llvm_project+llvm-project/llvm/lib/Demangle/Demangle.cpp";
    const MAIN_LABEL: &str = "@@//Editor/Document:Buffer.cpp";
    const MAIN_PATH: &str = "Editor/Document/Buffer.cpp";

    #[test]
    fn values_split_at_the_first_at_sign() {
        let parsed = PerLabelOptions::parse(r".*,-^external/.*,-^\x40.*@-Wall,-Wextra,-Werror").unwrap();
        assert_eq!(parsed.options, ["-Wall", "-Wextra", "-Werror"]);
        assert_eq!(parsed.filter.inclusions.len(), 1);
        assert_eq!(parsed.filter.exclusions.len(), 2);
        assert_eq!(PerLabelOptions::parse("a@-DX@Y").unwrap().options, ["-DX@Y"]);
    }

    #[test]
    fn a_backslash_keeps_a_comma_in_an_option_or_a_regex() {
        let parsed = PerLabelOptions::parse(r"b\,c@-DCOMMA\,ESCAPED,-DSECOND").unwrap();
        assert_eq!(parsed.options, ["-DCOMMA,ESCAPED", "-DSECOND"]);
        assert!(parsed.applies_to(label("@@//pkg:b,c.cc"), "pkg/b,c.cc").unwrap());
        assert_eq!(PerLabelOptions::parse(r"a@-DX\\,Y").unwrap().options, [r"-DX\,Y"]);
    }

    #[test]
    fn empty_parts_are_left_out() {
        assert_eq!(PerLabelOptions::parse("a@,-DX,,-DY,").unwrap().options, ["-DX", "-DY"]);
        assert!(PerLabelOptions::parse("a@").unwrap().options.is_empty());
        assert!(PerLabelOptions::parse("a").unwrap().options.is_empty());
        assert!(applies("@-DX", MAIN_LABEL, MAIN_PATH));
        assert!(applies("-@-DX", MAIN_LABEL, MAIN_PATH));
        assert!(!applies(r",a\.cc,@-DX", "@@//pkg:b.cc", "pkg/b.cc"));
    }

    #[test]
    fn regexes_match_anywhere_unless_anchored() {
        assert!(applies(r"a\.cc@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(applies(r"^pkg/a\.cc$@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(applies(r"^//pkg:a\.cc$@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(!applies(r"^pkg:a@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
    }

    #[test]
    fn main_repository_labels_have_no_at_signs() {
        assert!(!applies(r"^\x40\x40//pkg:a.*@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(applies(r".*,-^external/.*,-^\x40.*@-Wall", MAIN_LABEL, MAIN_PATH));
    }

    #[test]
    fn a_file_passes_by_its_label_or_its_path() {
        assert!(applies(r".*,-^external/.*@-Wall", LLVM_LABEL, LLVM_PATH));
        assert!(!applies(r".*,-^external/.*,-^\x40.*@-Wall", LLVM_LABEL, LLVM_PATH));
        assert!(applies(r"^\x40\x40\+.*@-Wall", LLVM_LABEL, LLVM_PATH));
        assert!(!applies(r"^\x40llvm.*@-Wall", LLVM_LABEL, LLVM_PATH));
        assert!(!applies(r".*,-.*Demangle.*@-Wall", LLVM_LABEL, LLVM_PATH));
        let parsed = PerLabelOptions::parse(r"^external/@-DX").unwrap();
        assert!(parsed.applies_to(None, LLVM_PATH).unwrap());
    }

    #[test]
    fn exclusions_alone_take_everything_else() {
        assert!(applies(r"-a\.cc@-DX", "@@//pkg:b.cc", "pkg/b.cc"));
        assert!(!applies(r"-a\.cc@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(!applies("-a,-b@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(!applies(r"\-x@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
    }

    #[test]
    fn regexes_are_javas() {
        assert!(applies(r"a(?=\.cc)@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(applies(r"(?>a)\.cc@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(applies(r"\d*@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
        assert!(applies(r"pk++g@-DX", "@@//pkg:a.cc", "pkg/a.cc"));
    }

    #[test]
    fn bad_filters_are_errors_as_bazel_words_them() {
        let error = PerLabelOptions::parse("[@-DX").unwrap_err();
        assert!(error.to_string().starts_with("Failed to build valid regular expression: "), "{error}");
        let error = PerLabelOptions::parse("--DX@-DX").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Failed to build filter: value looks like another flag (--DX). Either escape the value with \"\\-\\-\", or pass an explicit value to the flag."
        );
        assert!(PerLabelOptions::parse("a,--b@-DX").is_ok());
    }
}
