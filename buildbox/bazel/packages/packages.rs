//! Packages and their targets, as loading a BUILD file produces them.
//!
//! These are plain data, encoded compactly so that loaded packages can be
//! kept compressed, and independent of Starlark so that queries over them
//! don't need an interpreter.

mod attr;
mod rule_class;

pub use attr::{AttrType, AttrValue, Select, SelectorPart, DEFAULT_CONDITION};
pub use rule_class::{AttrDefault, AttrSchema, Cfg, RuleClass, RuleClassRef};

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
    pub targets: Vec<Target>,
    /// The `.bzl` files the BUILD file loads directly.
    pub loads: Vec<Label>,
    pub args: PackageArgs,
    /// Errors from evaluating the BUILD file. A package with errors still
    /// has the targets defined before the first error.
    pub errors: Vec<String>,
}

/// The settings `package()` sets for every target in a package.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct PackageArgs {
    pub default_visibility: Vec<Label>,
    pub default_testonly: bool,
    pub default_deprecation: Option<String>,
    pub features: Vec<String>,
    pub default_applicable_licenses: Vec<Label>,
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

impl Package {
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
    pub fn attr(&self, name: Symbol) -> Option<&AttrValue> {
        self.attrs.iter().find(|(attr, _)| *attr == name).map(|(_, value)| value)
    }
}
