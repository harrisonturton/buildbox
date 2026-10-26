//! Starlark errors as Bazel reports them: a syntax or name error at its
//! position, and an evaluation that failed with a traceback of the calls
//! it was in, worded as Bazel's Starlark words them.

use bazel_packages::{PackageError, SourcePosition, TraceFrame};
use starlark::codemap::FileSpan;
use starlark::ErrorKind;

/// `error` as Bazel reports it. `candidates` are the names in scope, which
/// an undefined name is checked against for a spelling suggestion.
pub fn package_error(error: &starlark::Error, candidates: &[String]) -> PackageError {
    let raw = error.without_diagnostic().to_string();
    let message = bazel_wording(&raw, candidates);
    if matches!(error.kind(), ErrorKind::Parser(_) | ErrorKind::Scope(_)) {
        return match error.span() {
            Some(span) => PackageError::at(start_position(span), message),
            None => PackageError::new(message),
        };
    }
    traceback(error, &raw, message)
}

/// A failed evaluation's traceback. Each call starlark-rust records names
/// the function called and where; Bazel's frames name the function making
/// each call instead, starting from the file's top level.
fn traceback(error: &starlark::Error, raw: &str, message: String) -> PackageError {
    let calls = &error.call_stack().frames;
    let mut frames = Vec::with_capacity(calls.len() + 1);
    let mut caller = "<toplevel>".to_string();
    for call in calls {
        if let Some(span) = &call.location {
            frames.push(frame(call_position(span), &caller, span));
        }
        caller = call.name.clone();
    }
    let in_last_call = match (error.span(), calls.last().and_then(|call| call.location.as_ref())) {
        (Some(span), Some(call)) => span == call,
        (None, _) => true,
        (Some(_), None) => false,
    };
    let message = match calls.last() {
        Some(call) if in_last_call => format!("Error in {}: {}", call.name, without_prefix(&message, &call.name)),
        _ => format!("Error: {message}"),
    };
    if !in_last_call {
        if let Some(span) = error.span() {
            frames.push(frame(error_position(span, raw), &caller, span));
        }
    }
    PackageError {
        position: None,
        traceback: frames,
        message,
    }
}

fn frame(position: SourcePosition, function: &str, span: &FileSpan) -> TraceFrame {
    let line = span.resolve_span().begin.line;
    TraceFrame {
        position,
        function: function.to_string(),
        source: span.file.source_line(line).trim().to_string(),
    }
}

/// `message` without the `name: ` that starlark-rust puts before a
/// function's own errors, as `fail` does.
fn without_prefix<'a>(message: &'a str, name: &str) -> &'a str {
    message.strip_prefix(name).and_then(|rest| rest.strip_prefix(": ")).unwrap_or(message)
}

/// Where a span starts.
pub fn start_position(span: &FileSpan) -> SourcePosition {
    let begin = span.resolve_span().begin;
    SourcePosition {
        file: span.filename().to_string(),
        line: begin.line as u32 + 1,
        column: begin.column as u32 + 1,
    }
}

/// Where an error in an expression is, as Bazel places it: at the
/// operator of a binary operation, or the `[` of an index, otherwise
/// where the expression starts.
fn error_position(span: &FileSpan, raw: &str) -> SourcePosition {
    let mut position = start_position(span);
    let source = span.source_span();
    let operator = raw.strip_prefix("Operation `").and_then(|rest| rest.split_once("` not supported for types")).map(|(operator, _)| operator);
    let offset = match operator {
        Some(operator) => unquoted_find(source, operator),
        None => index_bracket(source),
    };
    if let Some(offset) = offset {
        position.column += source[..offset].chars().count() as u32;
    }
    position
}

/// Where the `[` is of an index expression, such as `x["a"][b]`: the one
/// the final `]` closes.
fn index_bracket(source: &str) -> Option<usize> {
    if !source.ends_with(']') {
        return None;
    }
    let mut quote = None;
    let mut open = Vec::new();
    let mut closed = None;
    for (offset, character) in source.char_indices() {
        match quote {
            Some(start) if character == start => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\'' => quote = Some(character),
            None if character == '[' => open.push(offset),
            None if character == ']' => closed = open.pop(),
            None => {}
        }
    }
    closed.filter(|offset| *offset > 0)
}

/// Where `needle` first appears in `source` outside a string literal,
/// after its first character.
fn unquoted_find(source: &str, needle: &str) -> Option<usize> {
    let mut quote = None;
    for (offset, character) in source.char_indices() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\'' => quote = Some(character),
            None if offset > 0 && source[offset..].starts_with(needle) => return Some(offset),
            None => {}
        }
    }
    None
}

/// Where a call is, as Bazel places it: at its `(`.
pub fn call_position(span: &FileSpan) -> SourcePosition {
    let mut position = start_position(span);
    let source = span.source_span();
    if let Some(offset) = source.find('(').filter(|offset| !source[..*offset].contains('\n')) {
        position.column += source[..offset].chars().count() as u32;
    }
    position
}

/// A Starlark error's message as Bazel's Starlark words it, for the errors
/// whose wording differs.
pub fn bazel_wording(message: &str, candidates: &[String]) -> String {
    let message = message.trim();
    if let Some(name) = undefined_name(message) {
        return format!("name '{name}' is not defined{}", did_you_mean(name, candidates));
    }
    if let Some(rest) = message.strip_prefix("Parse error: unexpected ") {
        return syntax_error(rest).unwrap_or_else(|| format!("syntax error: {rest}"));
    }
    if let Some(error) = lexer_error(message) {
        return error;
    }
    if let Some(operation) = binary_operation(message) {
        return operation;
    }
    if let Some(key) = missing_key(message) {
        return format!("key {key} not found in dictionary");
    }
    message.strip_prefix("error: ").unwrap_or(message).to_string()
}

/// The name in starlark-rust's ``Variable `name` not found``, which may
/// end with its own suggestion.
fn undefined_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("Variable `")?;
    let (name, rest) = rest.split_once('`')?;
    rest.starts_with(" not found").then_some(name)
}

/// The key, as Starlark writes it, in starlark-rust's
/// ``Key `"name"` was not found``.
fn missing_key(message: &str) -> Option<&str> {
    message.strip_prefix("Key `")?.strip_suffix("` was not found")
}

/// `unsupported binary operation: int + string`, from starlark-rust's
/// ``Operation `+` not supported for types `int` and `string` ``.
fn binary_operation(message: &str) -> Option<String> {
    let rest = message.strip_prefix("Operation `")?;
    let (operator, rest) = rest.split_once("` not supported for types `")?;
    let (left, rest) = rest.split_once("` and `")?;
    let right = rest.strip_suffix('`')?;
    Some(format!("unsupported binary operation: {left} {operator} {right}"))
}

/// `syntax error at 'cmd': expected )`, from starlark-rust's
/// `identifier 'cmd', expected symbol ')'`, and Bazel's words for errors
/// of indentation.
fn syntax_error(unexpected: &str) -> Option<String> {
    let (found, expected) = unexpected.split_once(", expected ")?;
    if found == "new indentation block" {
        return Some("indentation error".to_string());
    }
    if expected == "new indentation block" {
        return Some("expected an indented block".to_string());
    }
    Some(format!("syntax error at '{}': expected {}", token_name(found), token_name(expected)))
}

/// A token as Bazel names it, from starlark-rust's description of it, such
/// as `identifier 'cmd'`, `symbol ')'` or `new line`: the quoted text, if
/// any.
fn token_name(description: &str) -> &str {
    if description == "new line" {
        return "newline";
    }
    let quoted = description.find('\'').zip(description.rfind('\''));
    match quoted {
        Some((open, close)) if open < close => &description[open + 1..close],
        _ => description,
    }
}

/// A lexer error as Bazel's lexer words it, from starlark-rust's.
fn lexer_error(message: &str) -> Option<String> {
    let rest = message.strip_prefix("Parse error: ")?;
    if let Some(input) = rest.strip_prefix("invalid input `").and_then(|input| input.strip_suffix('`')) {
        return Some(format!("invalid character: '{input}'"));
    }
    let worded = match rest {
        "unfinished string literal" => "unclosed string literal",
        "incorrect indentation" => "indentation error",
        _ => rest,
    };
    Some(worded.to_string())
}

/// ` (did you mean 'name'?)` for the candidate closest to `input`, if one
/// is close enough, as Bazel's `SpellChecker` chooses it; of candidates as
/// close as each other, the first in sorted order.
pub fn did_you_mean(input: &str, candidates: &[String]) -> String {
    let mut sorted: Vec<&String> = candidates.iter().collect();
    sorted.sort();
    let mut best = None;
    let mut best_distance = 5.min((input.chars().count() + 1) / 2);
    let input = input.to_lowercase();
    for candidate in sorted {
        let Some(distance) = edit_distance(&input, &candidate.to_lowercase(), best_distance) else {
            continue;
        };
        if distance < best_distance {
            best_distance = distance;
            best = Some(candidate);
        }
    }
    match best {
        Some(name) => format!(" (did you mean '{name}'?)"),
        None => String::new(),
    }
}

/// The Levenshtein distance between `a` and `b`, if it's at most `max`.
fn edit_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    if a == b {
        return Some(0);
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        row[0] = i;
        let mut best_in_row = row[0];
        let mut previous = i - 1;
        for j in 1..=b.len() {
            let old = row[j];
            let substitution = previous + usize::from(a[i - 1] != b[j - 1]);
            row[j] = substitution.min(1 + row[j - 1].min(row[j]));
            previous = old;
            best_in_row = best_in_row.min(row[j]);
        }
        if best_in_row > max {
            return None;
        }
    }
    Some(row[b.len()]).filter(|distance| *distance <= max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn close_names_are_suggested() {
        let candidates = names(&["select", "glob", "bool", "filegroup"]);
        assert_eq!(did_you_mean("selct", &candidates), " (did you mean 'select'?)");
        assert_eq!(did_you_mean("foo", &candidates), "");
        assert_eq!(did_you_mean("Glob", &candidates), " (did you mean 'glob'?)");
    }

    #[test]
    fn index_errors_are_at_the_last_index() {
        assert_eq!(index_bracket("x[\"missing\"]"), Some(1));
        assert_eq!(index_bracket("ctx.t[\"]\"][Info]"), Some(10));
        assert_eq!(index_bracket("[1, 2]"), None);
        assert_eq!(index_bracket("f(x)"), None);
    }

    #[test]
    fn operators_are_found_outside_strings() {
        assert_eq!(unquoted_find("1 + \"a+b\"", "+"), Some(2));
        assert_eq!(unquoted_find("\"+\" + 1", "+"), Some(4));
    }

    #[test]
    fn lexer_errors_are_worded_as_bazel_words_them() {
        assert_eq!(bazel_wording("Parse error: invalid input `$`", &[]), "invalid character: '$'");
        assert_eq!(bazel_wording("Parse error: unfinished string literal", &[]), "unclosed string literal");
        assert_eq!(bazel_wording("Parse error: unexpected new indentation block, expected expression", &[]), "indentation error");
        assert_eq!(bazel_wording("Parse error: unexpected identifier 'cc', expected new indentation block", &[]), "expected an indented block");
        assert_eq!(bazel_wording("Parse error: unexpected integer literal '2', expected new line", &[]), "syntax error at '2': expected newline");
    }

    #[test]
    fn messages_are_worded_as_bazel_words_them() {
        let candidates = names(&["select"]);
        assert_eq!(bazel_wording("Variable `selct` not found, did you mean `set`?", &candidates), "name 'selct' is not defined (did you mean 'select'?)");
        assert_eq!(bazel_wording("Operation `+` not supported for types `int` and `string`", &[]), "unsupported binary operation: int + string");
        assert_eq!(bazel_wording("Key `\"missing\"` was not found", &[]), "key \"missing\" not found in dictionary");
    }
}
