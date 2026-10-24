//! Attribute values and types.

use bazel_label::Label;
use skycodec::{Decode, Encode};

/// The condition a `select()` falls back to when no other matches.
pub const DEFAULT_CONDITION: &str = "//conditions:default";

/// The type of an attribute, which decides what values it accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum AttrType {
    Bool,
    Int,
    String,
    Label,
    /// A file the rule creates, named by a label in its package.
    Output,
    StringList,
    LabelList,
    OutputList,
    IntList,
    StringDict,
    StringListDict,
    LabelKeyedStringDict,
    StringKeyedLabelDict,
    /// `0`, `1`, or `-1` for "auto", as `stamp` uses.
    Tristate,
    License,
}

impl AttrType {
    /// The name Bazel uses for the type, as in `query --output=xml`.
    pub fn name(self) -> &'static str {
        match self {
            AttrType::Bool => "boolean",
            AttrType::Int => "int",
            AttrType::String => "string",
            AttrType::Label => "label",
            AttrType::Output => "output",
            AttrType::StringList => "string_list",
            AttrType::LabelList => "label_list",
            AttrType::OutputList => "output_list",
            AttrType::IntList => "int_list",
            AttrType::StringDict => "string_dict",
            AttrType::StringListDict => "string_list_dict",
            AttrType::LabelKeyedStringDict => "label_keyed_string_dict",
            AttrType::StringKeyedLabelDict => "string_keyed_label_dict",
            AttrType::Tristate => "tristate",
            AttrType::License => "license",
        }
    }

    /// Whether values of this type refer to other targets.
    pub fn has_labels(self) -> bool {
        matches!(
            self,
            AttrType::Label | AttrType::LabelList | AttrType::LabelKeyedStringDict | AttrType::StringKeyedLabelDict
        )
    }

    pub fn is_output(self) -> bool {
        matches!(self, AttrType::Output | AttrType::OutputList)
    }
}

/// An attribute's value.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AttrValue {
    None,
    Bool(bool),
    Int(i64),
    String(String),
    Label(Label),
    StringList(Vec<String>),
    LabelList(Vec<Label>),
    IntList(Vec<i64>),
    StringDict(Vec<(String, String)>),
    StringListDict(Vec<(String, Vec<String>)>),
    LabelKeyedStringDict(Vec<(Label, String)>),
    StringKeyedLabelDict(Vec<(String, Label)>),
    /// A value that depends on the configuration: plain values and
    /// `select()`s, concatenated.
    Configurable(Vec<SelectorPart>),
}

/// Part of a configurable value.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum SelectorPart {
    Value(AttrValue),
    Select(Select),
}

/// A `select()`: values chosen by which condition the configuration
/// matches.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Select {
    /// Each condition, a `config_setting` or [`DEFAULT_CONDITION`], with its
    /// value.
    pub branches: Vec<(Label, AttrValue)>,
    pub no_match_error: String,
}

impl AttrValue {
    /// Every label the value refers to, including in every branch of its
    /// `select()`s.
    pub fn labels(&self, out: &mut Vec<Label>) {
        match self {
            AttrValue::Label(label) => out.push(*label),
            AttrValue::LabelList(labels) => out.extend(labels),
            AttrValue::LabelKeyedStringDict(entries) => out.extend(entries.iter().map(|(label, _)| *label)),
            AttrValue::StringKeyedLabelDict(entries) => out.extend(entries.iter().map(|(_, label)| *label)),
            AttrValue::Configurable(parts) => {
                for part in parts {
                    match part {
                        SelectorPart::Value(value) => value.labels(out),
                        SelectorPart::Select(select) => {
                            for (_, value) in &select.branches {
                                value.labels(out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The conditions of the value's `select()`s.
    pub fn conditions(&self, out: &mut Vec<Label>) {
        let AttrValue::Configurable(parts) = self else {
            return;
        };
        for part in parts {
            if let SelectorPart::Select(select) = part {
                out.extend(select.branches.iter().map(|(condition, _)| *condition));
            }
        }
    }

    pub fn is_configurable(&self) -> bool {
        matches!(self, AttrValue::Configurable(_))
    }
}
