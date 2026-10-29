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
    /// The constraints every execution platform of the rule must have, as
    /// `rule(exec_compatible_with = ...)` gives them.
    pub exec_compatible_with: Vec<Label>,
    /// The exec groups the rule's actions can run in: those
    /// `rule(exec_groups = ...)` declares, and `test` for test rules.
    pub exec_groups: Vec<ExecGroup>,
    /// Whether `rule(cfg = ...)` gives the rule a transition, which changes
    /// the configuration its targets are analyzed in.
    pub incoming_transition: bool,
    /// For build setting rules: how their targets' values are set.
    pub build_setting: Option<BuildSettingSpec>,
    /// The providers `rule(provides = [...])` says its targets give, by
    /// key, which decide the aspects that apply to them.
    pub advertised_providers: Vec<String>,
    /// The calls that defined a Starlark rule class: where `rule()` was
    /// called, outermost first.
    pub defined_at: Vec<crate::CallFrame>,
    /// Whether it's an analysis test: a test whose implementation checks
    /// other targets' analysis, registering no actions of its own.
    pub analysis_test: bool,
}

/// An exec group: actions and tools that run on an execution platform of
/// their own, chosen for the group's own toolchains and constraints.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ExecGroup {
    pub name: Symbol,
    pub toolchain_types: Vec<Label>,
    /// The toolchain types among those that may go unresolved.
    pub optional_toolchain_types: Vec<Label>,
    pub exec_compatible_with: Vec<Label>,
}

/// How a build setting is set: whether it's a flag, which the command line
/// can set, and whether each use of the flag adds a value to a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct BuildSettingSpec {
    pub flag: bool,
    pub multiple: bool,
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
    /// For label attributes: the aspects applied to their dependencies.
    pub aspects: Vec<AspectRef>,
    /// What the dependencies of a Starlark rule's label attribute must be,
    /// which analysis checks; `None` for attributes bb doesn't check, such
    /// as native rules'.
    pub requirements: Option<Box<DepRequirements>>,
    /// Whether the value mustn't be empty, as `allow_empty = False` says.
    pub non_empty: bool,
}

/// What a label attribute's dependencies must be, as its `allow_files`,
/// `allow_single_file`, `allow_rules` and `providers` say.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct DepRequirements {
    /// The files it allows, by name.
    pub files: FileTypes,
    /// Whether each dependency must be one file, as `allow_single_file`
    /// says.
    pub single_file: bool,
    /// Whether the types of the files rules give aren't checked, as the
    /// flag `SKIP_ANALYSIS_TIME_FILETYPE_CHECK` asks.
    pub skip_file_type_check: bool,
    /// The kinds of rules it allows; any if empty.
    pub rules: Vec<String>,
    /// The providers a rule must have: all of one of these sets; any rule
    /// if empty.
    pub providers: Vec<Vec<ProviderRef>>,
}

/// The files a label attribute allows.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum FileTypes {
    Any,
    None,
    /// Files whose names end with one of these.
    Extensions(Vec<String>),
}

impl FileTypes {
    pub fn allows(&self, name: &str) -> bool {
        match self {
            FileTypes::Any => true,
            FileTypes::None => false,
            FileTypes::Extensions(extensions) => extensions.iter().any(|extension| name.ends_with(extension.as_str())),
        }
    }
}

/// A provider an attribute requires: its key, as targets are indexed by
/// it, and its name, as errors give it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ProviderRef {
    pub key: String,
    pub name: String,
}

/// An aspect, by the `.bzl` file that exports it and the name it's
/// exported as.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct AspectRef {
    pub file: Label,
    pub name: Symbol,
}

/// An attribute's value when a rule doesn't give one.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AttrDefault {
    Value(AttrValue),
    /// Computed from the rule's other attributes when it's analyzed.
    Computed,
    /// Chosen by the configuration when the rule is analyzed, as for
    /// `configuration_field()`.
    LateBound(LateBound),
}

/// A `configuration_field()`: a fragment's field naming a label that the
/// configuration chooses.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct LateBound {
    pub fragment: Symbol,
    pub name: Symbol,
    /// Its label with the default options, which `query` follows.
    pub default: Option<Label>,
}

/// The configuration an attribute's dependencies are built in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Cfg {
    Target,
    /// The execution platform of the exec group named, or of the rule.
    Exec(Option<Symbol>),
    /// A Starlark transition.
    Transition,
    /// `config.none()`: the configuration without options.
    None,
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
            aspects: Vec::new(),
            requirements: None,
            non_empty: false,
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
        self.cfg = Cfg::Exec(None);
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
