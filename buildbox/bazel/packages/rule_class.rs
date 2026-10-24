//! Rule classes: the kinds of rule, and the attributes they take.

use crate::{AttrType, AttrValue};
use bazel_label::Label;
use intern::Symbol;
use skycodec::{Decode, Encode};

/// Names a rule class: native, or exported from a `.bzl` file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RuleClassRef {
    /// The rule's kind, as `query` reports it, such as `cc_library`.
    pub kind: Symbol,
    /// The `.bzl` file that defines the rule and the name it's exported
    /// under, or `None` for native rules.
    pub definition: Option<(Label, Symbol)>,
}

/// A rule class's schema.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RuleClass {
    pub kind: Symbol,
    /// In the order Bazel lists them: the common attributes, then the
    /// class's own.
    pub attrs: Vec<AttrSchema>,
    pub test: bool,
    pub executable: bool,
    /// Outputs every rule of the class declares, as attribute name and file
    /// name template, such as `("out", "%{name}.txt")`.
    pub outputs: Vec<(Symbol, String)>,
    /// The toolchain types the rule's implementation resolves toolchains
    /// for, which `query` counts as dependencies.
    pub toolchain_types: Vec<Label>,
    /// The toolchain types among those that may go unresolved, for which
    /// the rule gets `None`.
    pub optional_toolchain_types: Vec<Label>,
}

/// One attribute a rule class takes.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AttrSchema {
    pub name: Symbol,
    pub ty: AttrType,
    pub default: AttrDefault,
    pub mandatory: bool,
    /// Whether the attribute may be given a `select()`.
    pub configurable: bool,
    pub cfg: Cfg,
    pub executable: bool,
    /// For label attributes: whether the labels are dependencies. The
    /// labels of `visibility`, for example, aren't.
    pub is_dependency: bool,
    /// For string attributes: the allowed values, if restricted.
    pub values: Vec<String>,
}

/// An attribute's value when a rule doesn't give one.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AttrDefault {
    Value(AttrValue),
    /// Computed from the rule's other attributes when it's analyzed.
    Computed,
    /// Chosen by the configuration when the rule is analyzed, as for
    /// `configuration_field()`.
    LateBound(Option<Label>),
}

/// The configuration an attribute's dependencies are built in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Cfg {
    Target,
    Exec,
    /// A Starlark transition.
    Transition,
}

impl RuleClass {
    pub fn attr(&self, name: Symbol) -> Option<&AttrSchema> {
        self.attrs.iter().find(|attr| attr.name == name)
    }

    pub fn is_private(attr: Symbol) -> bool {
        let name = attr.as_str();
        name.starts_with('_') || name.starts_with('$') || name.starts_with(':')
    }
}

impl AttrSchema {
    /// A non-mandatory attribute with the type's default value.
    pub fn new(name: &str, ty: AttrType) -> AttrSchema {
        AttrSchema {
            name: Symbol::intern(name),
            ty,
            default: AttrDefault::Value(AttrValue::default_for(ty)),
            mandatory: false,
            configurable: true,
            cfg: Cfg::Target,
            executable: false,
            is_dependency: ty.has_labels(),
            values: Vec::new(),
        }
    }

    pub fn mandatory(mut self) -> AttrSchema {
        self.mandatory = true;
        self
    }

    pub fn default(mut self, value: AttrValue) -> AttrSchema {
        self.default = AttrDefault::Value(value);
        self
    }

    pub fn nonconfigurable(mut self) -> AttrSchema {
        self.configurable = false;
        self
    }

    pub fn exec(mut self) -> AttrSchema {
        self.cfg = Cfg::Exec;
        self
    }

    pub fn not_dependency(mut self) -> AttrSchema {
        self.is_dependency = false;
        self
    }
}

impl AttrValue {
    /// The value an attribute of type `ty` has by default.
    pub fn default_for(ty: AttrType) -> AttrValue {
        match ty {
            AttrType::Bool => AttrValue::Bool(false),
            AttrType::Int => AttrValue::Int(0),
            AttrType::Tristate => AttrValue::Int(-1),
            AttrType::String => AttrValue::String(String::new()),
            AttrType::Label | AttrType::Output | AttrType::License => AttrValue::None,
            AttrType::StringList => AttrValue::StringList(Vec::new()),
            AttrType::LabelList | AttrType::OutputList => AttrValue::LabelList(Vec::new()),
            AttrType::IntList => AttrValue::IntList(Vec::new()),
            AttrType::StringDict => AttrValue::StringDict(Vec::new()),
            AttrType::StringListDict => AttrValue::StringListDict(Vec::new()),
            AttrType::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
            AttrType::StringKeyedLabelDict => AttrValue::StringKeyedLabelDict(Vec::new()),
        }
    }
}
