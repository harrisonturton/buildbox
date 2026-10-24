//! Rewriting Bazel Starlark that starlark-rust doesn't accept as written.

/// Methods Bazel lets take `default` by name, as in
/// `d.get(key, default = None)`, which starlark-rust only takes by
/// position.
const DEFAULT_BY_NAME: &[&str] = &[".get(", ".pop("];

/// Rewrite `.get(key, default = value)` and `.pop(...)` to pass `default`
/// by position, and `dict | select(...)` to `dict + select(...)`, since
/// starlark-rust has no reflected `|` for select to implement. Text in
/// strings and comments is left alone.
pub fn rewrite(source: &str) -> String {
    if !source.contains("default") && !source.contains("select(") {
        return source.to_string();
    }
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = skip_string_or_comment(bytes, index) {
            out.push_str(&source[index..end]);
            index = end;
            continue;
        }
        if is_union_with_select(&source[index..]) {
            out.push('+');
            index += 1;
            continue;
        }
        let Some(method) = DEFAULT_BY_NAME.iter().find(|method| source[index..].starts_with(*method)) else {
            let c = source[index..].chars().next().unwrap_or(' ');
            out.push(c);
            index += c.len_utf8();
            continue;
        };
        out.push_str(method);
        index += method.len();
        index = rewrite_call(source, index, &mut out);
    }
    out
}

/// Copy a call's arguments, up to its closing parenthesis, dropping
/// `default =` from its second argument.
fn rewrite_call(source: &str, start: usize, out: &mut String) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0;
    let mut commas = 0;
    let mut index = start;
    while index < bytes.len() {
        if let Some(end) = skip_string_or_comment(bytes, index) {
            out.push_str(&source[index..end]);
            index = end;
            continue;
        }
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => {
                out.push(')');
                return index + 1;
            }
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                commas += 1;
                out.push(',');
                index += 1;
                if commas == 1 {
                    index = skip_default_keyword(source, index, out);
                }
                continue;
            }
            _ => {}
        }
        let c = source[index..].chars().next().unwrap_or(' ');
        out.push(c);
        index += c.len_utf8();
    }
    index
}

/// After a call's first comma: skip `default =`, keeping the whitespace.
fn skip_default_keyword(source: &str, start: usize, out: &mut String) -> usize {
    let rest = &source[start..];
    let trimmed = rest.trim_start();
    let whitespace = &rest[..rest.len() - trimmed.len()];
    let Some(after) = trimmed.strip_prefix("default") else {
        return start;
    };
    let after_trimmed = after.trim_start();
    let Some(value) = after_trimmed.strip_prefix('=').filter(|value| !value.starts_with('=')) else {
        return start;
    };
    out.push_str(whitespace);
    let value_trimmed = value.trim_start();
    source.len() - value_trimmed.len()
}

/// Whether `rest` starts with a `|` whose right operand is a `select`.
fn is_union_with_select(rest: &str) -> bool {
    let Some(operand) = rest.strip_prefix('|') else {
        return false;
    };
    operand.trim_start().starts_with("select(")
}

/// The end of a string literal or comment starting at `index`, if one does.
fn skip_string_or_comment(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes[index] {
        b'#' => Some(bytes[index..].iter().position(|b| *b == b'\n').map(|end| index + end).unwrap_or(bytes.len())),
        b'"' | b'\'' => Some(string_end(bytes, index)),
        _ => None,
    }
}

fn string_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let triple = bytes.len() >= start + 3 && bytes[start + 1] == quote && bytes[start + 2] == quote;
    let mut index = start + if triple { 3 } else { 1 };
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b if b == quote && !triple => return index + 1,
            b if b == quote && bytes.len() >= index + 3 && bytes[index + 1] == quote && bytes[index + 2] == quote => return index + 3,
            b'\n' if !triple => return index,
            _ => index += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_default_by_position() {
        assert_eq!(rewrite("x = d.get(key, default = \"\")"), "x = d.get(key, \"\")");
        assert_eq!(rewrite("d.pop(f(a, b), default=None)"), "d.pop(f(a, b), None)");
        assert_eq!(rewrite("d.get(k, default)"), "d.get(k, default)");
        assert_eq!(rewrite("d.get(k, default == 1)"), "d.get(k, default == 1)");
        assert_eq!(rewrite("s = \".get(a, default = b)\""), "s = \".get(a, default = b)\"");
        assert_eq!(rewrite("# d.get(a, default = b)\nd.get(a, default = b)"), "# d.get(a, default = b)\nd.get(a, b)");
    }

    #[test]
    fn unions_with_select_by_concatenation() {
        assert_eq!(rewrite("x = {} | select({})"), "x = {} + select({})");
        assert_eq!(rewrite("x = a |\n    select({})"), "x = a +\n    select({})");
        assert_eq!(rewrite("x = a | b"), "x = a | b");
        assert_eq!(rewrite("x = \"| select(\""), "x = \"| select(\"");
    }
}
