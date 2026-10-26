//! Protocol buffer messages, built field by field and written in the
//! three forms query commands print them in: the binary wire format,
//! JSON, as `JsonFormat` writes it, and text format.
//!
//! The messages are those of Bazel's `build.proto` and `analysis_v2.proto`,
//! which their writers give fields of by number and name; fields are
//! written in order of number, as protobuf writes them.

use std::fmt::Write;

/// A message: its fields in the order they're written, with a repeated
/// field's values next to each other.
#[derive(Clone, Debug, Default)]
pub struct Message {
    fields: Vec<Field>,
}

#[derive(Clone, Debug)]
struct Field {
    number: u32,
    name: &'static str,
    value: Value,
    /// Whether the field is repeated, which JSON writes as an array.
    repeated: bool,
}

/// A field's value.
#[derive(Clone, Debug)]
enum Value {
    String(String),
    /// An `int32`, `uint32` or `int64`; `int64`s are strings in JSON.
    Int { value: i64, wide: bool },
    Bool(bool),
    Enum { number: i32, name: &'static str },
    Message(Message),
    /// A repeated scalar field of `proto3`, which is packed on the wire.
    Packed(Vec<i64>),
}

impl Message {
    pub fn new() -> Message {
        Message::default()
    }

    fn push(&mut self, number: u32, name: &'static str, value: Value) -> &mut Message {
        self.fields.push(Field {
            number,
            name,
            value,
            repeated: false,
        });
        self
    }

    fn push_repeated(&mut self, number: u32, name: &'static str, value: Value) -> &mut Message {
        self.fields.push(Field {
            number,
            name,
            value,
            repeated: true,
        });
        self
    }

    pub fn string(&mut self, number: u32, name: &'static str, value: impl Into<String>) -> &mut Message {
        self.push(number, name, Value::String(value.into()))
    }

    pub fn strings<S: Into<String>>(&mut self, number: u32, name: &'static str, values: impl IntoIterator<Item = S>) -> &mut Message {
        for value in values {
            self.push_repeated(number, name, Value::String(value.into()));
        }
        self
    }

    pub fn int(&mut self, number: u32, name: &'static str, value: i64) -> &mut Message {
        self.push(number, name, Value::Int { value, wide: false })
    }

    pub fn int64(&mut self, number: u32, name: &'static str, value: i64) -> &mut Message {
        self.push(number, name, Value::Int { value, wide: true })
    }

    pub fn ints(&mut self, number: u32, name: &'static str, values: impl IntoIterator<Item = i64>) -> &mut Message {
        for value in values {
            self.push_repeated(number, name, Value::Int { value, wide: false });
        }
        self
    }

    /// A `proto3` repeated integer field, packed on the wire; nothing if
    /// it's empty.
    pub fn packed(&mut self, number: u32, name: &'static str, values: Vec<i64>) -> &mut Message {
        if values.is_empty() {
            return self;
        }
        self.push_repeated(number, name, Value::Packed(values))
    }

    pub fn bool(&mut self, number: u32, name: &'static str, value: bool) -> &mut Message {
        self.push(number, name, Value::Bool(value))
    }

    pub fn enumeration(&mut self, number: u32, name: &'static str, value: i32, value_name: &'static str) -> &mut Message {
        self.push(number, name, Value::Enum { number: value, name: value_name })
    }

    pub fn message(&mut self, number: u32, name: &'static str, value: Message) -> &mut Message {
        self.push(number, name, Value::Message(value))
    }

    /// A value of a repeated message field.
    pub fn repeated(&mut self, number: u32, name: &'static str, value: Message) -> &mut Message {
        self.push_repeated(number, name, Value::Message(value))
    }

    /// A `proto3` string, which isn't written if it's empty.
    pub fn string3(&mut self, number: u32, name: &'static str, value: impl Into<String>) -> &mut Message {
        let value = value.into();
        if value.is_empty() {
            return self;
        }
        self.string(number, name, value)
    }

    /// A `proto3` integer, which isn't written if it's zero.
    pub fn int3(&mut self, number: u32, name: &'static str, value: i64) -> &mut Message {
        if value == 0 {
            return self;
        }
        self.int(number, name, value)
    }

    /// A `proto3` boolean, which isn't written if it's false.
    pub fn bool3(&mut self, number: u32, name: &'static str, value: bool) -> &mut Message {
        if !value {
            return self;
        }
        self.bool(number, name, value)
    }

    /// Whether the message has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The message in the wire format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for field in self.in_order() {
            match &field.value {
                Value::String(text) => length_delimited(out, field.number, text.as_bytes()),
                Value::Int { value, .. } => varint_field(out, field.number, *value as u64),
                Value::Bool(value) => varint_field(out, field.number, u64::from(*value)),
                Value::Enum { number, .. } => varint_field(out, field.number, *number as i64 as u64),
                Value::Message(message) => length_delimited(out, field.number, &message.to_bytes()),
                Value::Packed(values) => {
                    let mut packed = Vec::new();
                    for value in values {
                        varint(&mut packed, *value as u64);
                    }
                    length_delimited(out, field.number, &packed);
                }
            }
        }
    }

    /// The message as `JsonFormat` prints it, indented, or on one line if
    /// `compact`.
    pub fn to_json(&self, compact: bool) -> String {
        let mut out = String::new();
        self.write_json(&mut out, 0, compact);
        out
    }

    fn write_json(&self, out: &mut String, depth: usize, compact: bool) {
        if self.fields.is_empty() {
            out.push('{');
            newline(out, depth, compact);
            out.push('}');
            return;
        }
        out.push('{');
        let groups = self.groups();
        for (index, group) in groups.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            newline(out, depth + 1, compact);
            write!(out, "\"{}\":", camel_case(group[0].name)).ok();
            if !compact {
                out.push(' ');
            }
            match group[0].repeated {
                true => write_json_array(group, out, depth + 1, compact),
                false => group[0].value.write_json(out, depth + 1, compact),
            }
        }
        newline(out, depth, compact);
        out.push('}');
    }

    /// The fields in the order protobuf writes them: by number, a repeated
    /// field's values in the order they were given.
    fn in_order(&self) -> Vec<&Field> {
        let mut fields: Vec<&Field> = self.fields.iter().collect();
        fields.sort_by_key(|field| field.number);
        fields
    }

    /// The fields, grouped by number, in order.
    fn groups(&self) -> Vec<Vec<&Field>> {
        let mut groups: Vec<Vec<&Field>> = Vec::new();
        for field in self.in_order() {
            match groups.iter_mut().find(|group| group[0].number == field.number) {
                Some(group) => group.push(field),
                None => groups.push(vec![field]),
            }
        }
        groups
    }

    /// The message in text format.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write_text(&mut out, 0, 0);
        out
    }

    /// The message in text format, the fields of its messages not
    /// indented, but those of messages within them, as `aquery` writes
    /// each entry of its graph.
    pub fn to_flat_text(&self) -> String {
        let mut out = String::new();
        self.write_text(&mut out, 0, 1);
        out
    }

    /// Write the fields of a message `depth` messages deep, indented by
    /// two spaces for each level but the first `unindented`.
    fn write_text(&self, out: &mut String, depth: usize, unindented: usize) {
        let indent = "  ".repeat(depth.saturating_sub(unindented));
        for field in self.in_order() {
            match &field.value {
                Value::Message(message) => {
                    writeln!(out, "{indent}{} {{", field.name).ok();
                    message.write_text(out, depth + 1, unindented);
                    writeln!(out, "{indent}}}").ok();
                }
                Value::Packed(values) => {
                    for value in values {
                        writeln!(out, "{indent}{}: {value}", field.name).ok();
                    }
                }
                value => {
                    write!(out, "{indent}{}: ", field.name).ok();
                    value.write_text(out);
                    out.push('\n');
                }
            }
        }
    }
}

impl Value {
    fn write_json(&self, out: &mut String, depth: usize, compact: bool) {
        match self {
            Value::String(text) => json_string(out, text),
            Value::Int { value, wide: true } => write!(out, "\"{value}\"").unwrap_or_default(),
            Value::Int { value, wide: false } => write!(out, "{value}").unwrap_or_default(),
            Value::Bool(value) => write!(out, "{value}").unwrap_or_default(),
            Value::Enum { name, .. } => write!(out, "\"{name}\"").unwrap_or_default(),
            Value::Message(message) => message.write_json(out, depth, compact),
            Value::Packed(values) => {
                let values: Vec<String> = values.iter().map(i64::to_string).collect();
                let separator = if compact { "," } else { ", " };
                write!(out, "[{}]", values.join(separator)).ok();
            }
        }
    }

    fn write_text(&self, out: &mut String) {
        match self {
            Value::String(text) => text_string(out, text),
            Value::Int { value, .. } => write!(out, "{value}").unwrap_or_default(),
            Value::Bool(value) => write!(out, "{value}").unwrap_or_default(),
            Value::Enum { name, .. } => out.push_str(name),
            Value::Message(_) | Value::Packed(_) => {}
        }
    }
}

/// A repeated field's values as a JSON array: messages one after another,
/// scalars on one line.
fn write_json_array(values: &[&Field], out: &mut String, depth: usize, compact: bool) {
    if let [field] = values {
        if let Value::Packed(_) = field.value {
            field.value.write_json(out, depth, compact);
            return;
        }
    }
    out.push('[');
    for (index, field) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
            if !compact {
                out.push(' ');
            }
        }
        field.value.write_json(out, depth, compact);
    }
    out.push(']');
}

fn newline(out: &mut String, depth: usize, compact: bool) {
    if compact {
        return;
    }
    out.push('\n');
    out.push_str(&"  ".repeat(depth));
}

/// A field's JSON name: its name in lower camel case.
fn camel_case(name: &str) -> String {
    let mut camel = String::with_capacity(name.len());
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            camel.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            camel.push(c);
        }
    }
    camel
}

/// A JSON string, escaped as Gson does, HTML characters and all.
fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' => write!(out, "\\u{:04x}", c as u32).unwrap_or_default(),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => write!(out, "\\u{:04x}", c as u32).unwrap_or_default(),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A text format string, escaped as protobuf's `TextFormat` does.
fn text_string(out: &mut String, text: &str) {
    out.push('"');
    for byte in text.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\'' => out.push_str("\\'"),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(byte as char),
            byte => write!(out, "\\{byte:03o}").unwrap_or_default(),
        }
    }
    out.push('"');
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint_field(out: &mut Vec<u8>, number: u32, value: u64) {
    varint(out, u64::from(number) << 3);
    varint(out, value);
}

fn length_delimited(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
    varint(out, (u64::from(number) << 3) | 2);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A message prefixed with its length, as streamed protos are written.
pub fn delimited(message: &Message) -> Vec<u8> {
    let bytes = message.to_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 5);
    varint(&mut out, bytes.len() as u64);
    out.extend_from_slice(&bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Message {
        let mut attribute = Message::new();
        attribute.string(1, "name", "srcs").enumeration(2, "type", 6, "LABEL_LIST").strings(6, "string_list_value", ["//:a", "//:b"]);
        let mut rule = Message::new();
        rule.string(1, "name", "//:a").repeated(4, "attribute", attribute);
        rule
    }

    #[test]
    fn writes_the_wire_format() {
        let mut message = Message::new();
        message.int(1, "id", 150).string(2, "name", "a").int(3, "negative", -1);
        let bytes = message.to_bytes();
        assert_eq!(&bytes[..5], &[0x08, 0x96, 0x01, 0x12, 0x01]);
        assert_eq!(bytes.len(), 5 + 1 + 1 + 10);
    }

    #[test]
    fn writes_json_as_json_format_does() {
        let json = rule().to_json(false);
        assert_eq!(
            json,
            "{\n  \"name\": \"//:a\",\n  \"attribute\": [{\n    \"name\": \"srcs\",\n    \"type\": \"LABEL_LIST\",\n    \"stringListValue\": [\"//:a\", \"//:b\"]\n  }]\n}"
        );
        assert_eq!(rule().to_json(true), "{\"name\":\"//:a\",\"attribute\":[{\"name\":\"srcs\",\"type\":\"LABEL_LIST\",\"stringListValue\":[\"//:a\",\"//:b\"]}]}");
        let mut escaped = String::new();
        json_string(&mut escaped, "a > b");
        assert_eq!(escaped, "\"a \\u003e b\"");
    }

    #[test]
    fn writes_text_format() {
        assert_eq!(rule().to_text(), "name: \"//:a\"\nattribute {\n  name: \"srcs\"\n  type: LABEL_LIST\n  string_list_value: \"//:a\"\n  string_list_value: \"//:b\"\n}\n");
    }
}
