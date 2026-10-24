//! Bazel's `json` module.

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, Value};

/// Re-indent compact JSON as Go's `json.Indent` does, which Bazel's
/// `json.indent` follows: each element on its own line, nested `indent`
/// deeper, each line starting with `prefix`. Empty arrays and objects stay
/// on one line.
pub fn indent(json: &str, prefix: &str, indent: &str) -> String {
    let mut out = String::with_capacity(json.len() * 2);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = json.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(prefix);
        for _ in 0..depth {
            out.push_str(indent);
        }
    };
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                let close = if c == '{' { '}' } else { ']' };
                if chars.peek() == Some(&close) {
                    out.push(close);
                    chars.next();
                    continue;
                }
                depth += 1;
                newline(&mut out, depth);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn to_starlark<'v>(value: &serde_json::Value, heap: Heap<'v>) -> Value<'v> {
    match value {
        serde_json::Value::Null => Value::new_none(),
        serde_json::Value::Bool(b) => Value::new_bool(*b),
        serde_json::Value::Number(n) => match n.as_i64().and_then(|i| i32::try_from(i).ok()) {
            Some(i) => heap.alloc(i),
            None => heap.alloc(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => heap.alloc(s.as_str()),
        serde_json::Value::Array(items) => heap.alloc(AllocList(items.iter().map(|item| to_starlark(item, heap)))),
        serde_json::Value::Object(entries) => heap.alloc(AllocDict(entries.iter().map(|(k, v)| (k.as_str(), to_starlark(v, heap))))),
    }
}

#[starlark_module]
fn json_functions(builder: &mut GlobalsBuilder) {
    fn encode<'v>(#[starlark(require = pos)] x: Value<'v>) -> anyhow::Result<String> {
        Ok(x.to_json()?)
    }

    fn decode<'v>(#[starlark(require = pos)] x: &str, default: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        match serde_json::from_str::<serde_json::Value>(x) {
            Ok(value) => Ok(to_starlark(&value, heap)),
            Err(_) if default.is_some() => Ok(default.unwrap_or_else(Value::new_none)),
            Err(error) => anyhow::bail!("json.decode: {error}"),
        }
    }

    fn indent(
        #[starlark(require = pos)] s: &str,
        #[starlark(require = named, default = "")] prefix: &str,
        #[starlark(require = named, default = "\t")] indent: &str,
    ) -> anyhow::Result<String> {
        Ok(self::indent(s, prefix, indent))
    }

    fn encode_indent<'v>(
        #[starlark(require = pos)] x: Value<'v>,
        #[starlark(require = named, default = "")] prefix: &str,
        #[starlark(require = named, default = "\t")] indent: &str,
    ) -> anyhow::Result<String> {
        Ok(self::indent(&x.to_json()?, prefix, indent))
    }
}

/// Add the `json` namespace.
pub(crate) fn register(builder: &mut GlobalsBuilder) {
    builder.namespace("json", json_functions);
}

#[cfg(test)]
mod tests {
    #[test]
    fn indents_like_go() {
        let json = r#"{"a":[1,2],"b":{},"c":"x,{y}"}"#;
        assert_eq!(super::indent(json, "", "  "), "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": \"x,{y}\"\n}");
    }
}
