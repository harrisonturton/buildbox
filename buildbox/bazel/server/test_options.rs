//! The test options that choose how many shards, runs and attempts each
//! test gets: `--test_sharding_strategy`, `--runs_per_test` and
//! `--flaky_test_attempts`.

use bazel_options::ParsedOptions;
use regex::Regex;

/// How a command's options say to split, repeat and retry its tests.
pub struct TestRepetition {
    sharding: Sharding,
    runs: Vec<PerLabel<u32>>,
    attempts: Vec<PerLabel<Attempts>>,
}

/// `--test_sharding_strategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sharding {
    /// Shard tests as their `shard_count` says.
    Explicit,
    Disabled,
    /// Split every test into this many shards.
    Forced(u32),
}

/// `--flaky_test_attempts`' value for some tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempts {
    /// Three attempts for tests marked `flaky`, one for others.
    Default,
    Count(u32),
}

/// A value for the tests whose labels a filter matches, as Bazel's
/// `regex_filter@value` options give them.
struct PerLabel<T> {
    filter: RegexFilter,
    value: T,
}

/// Regular expressions that a label must match one of, if there are any,
/// and must match none of those prefixed with `-`.
#[derive(Default)]
struct RegexFilter {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl RegexFilter {
    fn parse(text: &str, option: &str) -> Result<RegexFilter, String> {
        let mut filter = RegexFilter::default();
        for pattern in text.split(',').filter(|pattern| !pattern.is_empty()) {
            let (list, pattern) = match pattern.strip_prefix('-') {
                Some(excluded) => (&mut filter.exclude, excluded),
                None => (&mut filter.include, pattern),
            };
            let regex = Regex::new(pattern).map_err(|error| format!("While parsing option --{option}={text}: Failed to build valid regular expression: {error}"))?;
            list.push(regex);
        }
        Ok(filter)
    }

    fn matches(&self, label: &str) -> bool {
        let included = self.include.is_empty() || self.include.iter().any(|regex| regex.is_match(label));
        included && !self.exclude.iter().any(|regex| regex.is_match(label))
    }
}

impl<T: Copy> PerLabel<T> {
    /// Parse `regex_filter@value`, or `value` for every test.
    fn parse(text: &str, option: &str, value: impl Fn(&str) -> Result<T, String>) -> Result<PerLabel<T>, String> {
        let (filter, value_text) = match text.rsplit_once('@') {
            Some((filter, value)) => (RegexFilter::parse(filter, option)?, value),
            None => (RegexFilter::default(), text),
        };
        Ok(PerLabel { filter, value: value(value_text)? })
    }

    /// The value of the last of `options` whose filter matches `label`.
    fn find(options: &[PerLabel<T>], label: &str) -> Option<T> {
        options.iter().rev().find(|option| option.filter.matches(label)).map(|option| option.value)
    }
}

impl TestRepetition {
    pub fn parse(options: &ParsedOptions) -> Result<TestRepetition, String> {
        let sharding = parse_sharding(options.get("test_sharding_strategy").unwrap_or("explicit"))?;
        let mut runs = Vec::new();
        for text in options.all("runs_per_test") {
            runs.push(PerLabel::parse(text, "runs_per_test", |value| positive(value, "runs_per_test", text))?);
        }
        let mut attempts = Vec::new();
        for text in options.all("flaky_test_attempts") {
            attempts.push(PerLabel::parse(text, "flaky_test_attempts", |value| parse_attempts(value, text))?);
        }
        Ok(TestRepetition { sharding, runs, attempts })
    }

    /// How many shards a test whose `shard_count` is `shard_count` gets.
    pub fn shards(&self, shard_count: u32) -> u32 {
        match self.sharding {
            Sharding::Explicit => shard_count,
            Sharding::Disabled => 0,
            Sharding::Forced(shards) => shards,
        }
    }

    /// How many times the test `label` runs.
    pub fn runs(&self, label: &str) -> u32 {
        PerLabel::find(&self.runs, label).unwrap_or(1)
    }

    /// How many times the test `label` is tried before it fails.
    pub fn attempts(&self, label: &str, flaky: bool) -> u32 {
        match PerLabel::find(&self.attempts, label).unwrap_or(Attempts::Default) {
            Attempts::Count(count) => count,
            Attempts::Default if flaky => 3,
            Attempts::Default => 1,
        }
    }
}

fn parse_sharding(text: &str) -> Result<Sharding, String> {
    match text {
        "explicit" => Ok(Sharding::Explicit),
        "disabled" => Ok(Sharding::Disabled),
        _ => text
            .strip_prefix("forced=")
            .and_then(|count| count.parse::<u32>().ok())
            .filter(|count| *count > 0)
            .map(Sharding::Forced)
            .ok_or_else(|| format!("While parsing option --test_sharding_strategy={text}: Not a valid test sharding strategy: '{text}'")),
    }
}

fn parse_attempts(value: &str, text: &str) -> Result<Attempts, String> {
    if value == "default" {
        return Ok(Attempts::Default);
    }
    match value.parse::<u32>() {
        Ok(count) if (1..=10).contains(&count) => Ok(Attempts::Count(count)),
        _ => Err(format!("While parsing option --flaky_test_attempts={text}: '{value}' should be an integer between 1 and 10, or 'default'")),
    }
}

fn positive(value: &str, option: &str, text: &str) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(count) if count > 0 => Ok(count),
        _ => Err(format!("While parsing option --{option}={text}: '{value}' should be a positive integer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_matching_filter_wins() {
        let parse = |text: &str| PerLabel::parse(text, "runs_per_test", |value| positive(value, "runs_per_test", text)).unwrap();
        let runs = vec![parse("3"), parse("//foo/.*,-//foo/bar.*@5"), parse("//other@7")];
        assert_eq!(PerLabel::find(&runs, "//foo/baz:t"), Some(5));
        assert_eq!(PerLabel::find(&runs, "//foo/bar:t"), Some(3));
        assert_eq!(PerLabel::find(&runs, "//other:t"), Some(7));
    }

    #[test]
    fn parses_sharding_strategies() {
        assert_eq!(parse_sharding("explicit"), Ok(Sharding::Explicit));
        assert_eq!(parse_sharding("forced=4"), Ok(Sharding::Forced(4)));
        assert!(parse_sharding("forced=0").is_err());
        assert!(parse_sharding("sometimes").is_err());
    }
}
