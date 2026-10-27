//! Packages and their targets, as loading a BUILD file produces them.
//!
//! These are plain data, encoded compactly so that loaded packages can be
//! kept compressed, and independent of Starlark so that queries over them
//! don't need an interpreter.

mod attr;
mod error;
mod rule_class;
mod tests_of;

pub use attr::{AttrType, AttrValue, Select, SelectorPart, DEFAULT_CONDITION};
pub use error::{CallFrame, PackageError, SourcePosition, TraceFrame};
pub use rule_class::{AspectRef, AttrDefault, AttrSchema, BuildSettingSpec, Cfg, DepRequirements, FileTypes, LateBound, ProviderRef, RuleClass, RuleClassRef};
pub use tests_of::{TagFilter, is_manual, is_test_kind, passes_tag_filters};

use bazel_label::{Label, PackageId};
use intern::Symbol;
use skycodec::{Decode, Encode};

/// A loaded package: its targets, sorted by name, and what loading it read.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Package {
    pub id: PackageId,
    /// The BUILD file, as a target of the package.
    pub build_file: Label,
    /// The rule classes the package's rules are instances of. Rules refer
    /// to them by index.
    pub rule_classes: Vec<RuleClassRef>,
    /// The schemas of rule classes the BUILD file itself defines, with
    /// `testing.analysis_test`, which its rules' classes name by its label.
    pub defined_classes: Vec<RuleClass>,
    pub targets: Vec<Target>,
    /// The `.bzl` files the BUILD file loads directly.
    pub loads: Vec<Label>,
    pub args: PackageArgs,
    /// Errors from evaluating the BUILD file. A package with errors still
    /// has the targets defined before evaluation failed.
    pub errors: Vec<PackageError>,
}

/// The settings `package()` sets for every target in a package.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct PackageArgs {
    pub default_visibility: Vec<Label>,
    pub default_testonly: bool,
    pub default_deprecation: Option<String>,
    pub features: Vec<String>,
    pub default_applicable_licenses: Vec<Label>,
    pub default_package_metadata: Vec<Label>,
}

/// Something a label can name.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Target {
    Rule(Rule),
    /// A file in the source tree.
    SourceFile(SourceFile),
    /// A file a rule declares as an output.
    OutputFile { name: Symbol, rule: Symbol },
    PackageGroup(PackageGroup),
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct SourceFile {
    pub name: Symbol,
    /// Visibility given by `exports_files`, or `None` for files that are
    /// only visible within their package.
    pub visibility: Option<Vec<Label>>,
}

/// An instance of a rule class.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Rule {
    pub name: Symbol,
    /// The index of the rule's class in [`Package::rule_classes`].
    pub class: u32,
    /// The attributes given explicitly, in the order given.
    pub attrs: Vec<(Symbol, AttrValue)>,
    pub location: Location,
    /// The macro that created the rule, if any.
    pub generator: Option<Generator>,
    /// The names of the files the rule declares as outputs.
    pub outputs: Vec<Symbol>,
    /// The values of attributes with computed defaults that weren't given,
    /// as computed when the rule was created.
    pub computed: Vec<(Symbol, AttrValue)>,
    /// The calls that created the rule, outermost first, when a macro
    /// created it; a rule the BUILD file creates was created at its
    /// location.
    pub call_stack: Vec<CallFrame>,
    /// The symbolic macros that declared the rule, outermost first, which
    /// its visibility is checked against.
    pub macros: Vec<MacroFrame>,
}

/// A symbolic macro that declared a target: the package of the `.bzl`
/// file that defines it, and the labels its caller gave it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct MacroFrame {
    pub definition: PackageId,
    pub labels: Vec<Label>,
}

/// Where in the BUILD file a rule was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Location {
    pub line: u32,
    pub column: u32,
}

/// The macro call in the BUILD file that created a rule.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Generator {
    pub function: Symbol,
    pub name: Symbol,
    pub location: Location,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PackageGroup {
    pub name: Symbol,
    pub packages: Vec<String>,
    pub includes: Vec<Label>,
}

/// A label of a target's visibility as Bazel keeps it:
/// `//visibility:public` and `//visibility:private` mean the same in any
/// repository, so they're kept as the main repository's.
pub fn visibility_label(label: Label) -> Label {
    let constant = label.package.path.to_string() == "visibility" && matches!(label.name.as_str(), "public" | "private");
    match constant {
        true => PackageId::new(bazel_label::RepoName::MAIN, label.package.path).label(label.name),
        false => label,
    }
}

/// Rules without the attribute package defaults such as licenses apply
/// to, as Bazel 9 defines them.
const DEPENDENCY_RESOLUTION_RULES: [&str; 3] = ["constraint_setting", "constraint_value", "platform"];

impl Package {
    /// The licenses `package(default_applicable_licenses = ...)` applies to
    /// a rule of the package that doesn't set its own, unless it's one of
    /// them or a rule used to resolve dependencies.
    pub fn default_licenses(&self, rule: &Rule) -> Vec<Label> {
        let defaults = &self.args.default_applicable_licenses;
        let kind = self.rule_class(rule).kind;
        if defaults.is_empty() || DEPENDENCY_RESOLUTION_RULES.contains(&kind.as_str()) || defaults.contains(&self.label(rule.name)) {
            return Vec::new();
        }
        match rule.attr(Symbol::intern("applicable_licenses")) {
            Some(_) => Vec::new(),
            None => defaults.clone(),
        }
    }

    pub fn target(&self, name: Symbol) -> Option<&Target> {
        let index = self.targets.binary_search_by(|target| target.name().cmp(&name)).ok()?;
        Some(&self.targets[index])
    }

    pub fn label(&self, name: Symbol) -> Label {
        self.id.label(name)
    }

    pub fn rule_class(&self, rule: &Rule) -> &RuleClassRef {
        &self.rule_classes[rule.class as usize]
    }

    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.targets.iter().filter_map(|target| match target {
            Target::Rule(rule) => Some(rule),
            _ => None,
        })
    }
}

impl Target {
    pub fn name(&self) -> Symbol {
        match self {
            Target::Rule(rule) => rule.name,
            Target::SourceFile(file) => file.name,
            Target::OutputFile { name, .. } => *name,
            Target::PackageGroup(group) => group.name,
        }
    }

    pub fn as_rule(&self) -> Option<&Rule> {
        match self {
            Target::Rule(rule) => Some(rule),
            _ => None,
        }
    }
}

impl Rule {
    /// An attribute's value, if given explicitly.
    pub fn attr(&self, name: Symbol) -> Option<&AttrValue> {
        self.attrs.iter().find(|(attr, _)| *attr == name).map(|(_, value)| value)
    }

    /// An attribute's value, given explicitly or computed by its default.
    pub fn attr_or_computed(&self, name: Symbol) -> Option<&AttrValue> {
        self.attr(name)
            .or_else(|| self.computed.iter().find(|(attr, _)| *attr == name).map(|(_, value)| value))
    }
}
