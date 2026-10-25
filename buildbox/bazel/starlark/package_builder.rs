//! Accumulates the targets a BUILD file defines.

use bazel_label::{Label, PackageId};
use bazel_packages::{Package, PackageArgs, PackageGroup, Rule, RuleClassRef, SourceFile, Target};
use intern::Symbol;
use std::collections::{BTreeMap, HashMap};

/// The package a BUILD file is defining, as it is defined.
pub struct PackageBuilder {
    pub id: PackageId,
    pub build_file_name: Symbol,
    pub args: PackageArgs,
    pub loads: Vec<Label>,
    /// Whether `package()` has been called, which must happen before any
    /// target is defined.
    pub package_called: bool,
    pub errors: Vec<String>,
    rule_classes: Vec<RuleClassRef>,
    rule_class_index: HashMap<RuleClassRef, u32>,
    targets: BTreeMap<Symbol, Target>,
}

impl PackageBuilder {
    pub fn new(id: PackageId, build_file_name: Symbol) -> PackageBuilder {
        PackageBuilder {
            id,
            build_file_name,
            args: PackageArgs::default(),
            loads: Vec::new(),
            package_called: false,
            errors: Vec::new(),
            rule_classes: Vec::new(),
            rule_class_index: HashMap::new(),
            targets: BTreeMap::new(),
        }
    }

    pub fn target(&self, name: Symbol) -> Option<&Target> {
        self.targets.get(&name)
    }

    pub fn targets(&self) -> impl Iterator<Item = &Target> {
        self.targets.values()
    }

    pub fn rule_class(&self, rule: &Rule) -> &RuleClassRef {
        &self.rule_classes[rule.class as usize]
    }

    /// Add a rule of the given class, and the output files it declares.
    pub fn add_rule(&mut self, class: RuleClassRef, mut rule: Rule) -> anyhow::Result<()> {
        self.check_name_free(rule.name, "rule")?;
        for output in &rule.outputs {
            if *output == rule.name {
                anyhow::bail!("rule '{}' has an output file with the same name as the rule", rule.name);
            }
            self.check_name_free(*output, "output file")?;
        }
        rule.class = self.intern_class(class);
        for output in &rule.outputs {
            self.targets.insert(*output, Target::OutputFile { name: *output, rule: rule.name });
        }
        self.targets.insert(rule.name, Target::Rule(rule));
        Ok(())
    }

    /// Make files of the package visible to other packages, as
    /// `exports_files` does.
    pub fn export_file(&mut self, name: Symbol, visibility: Option<Vec<Label>>) -> anyhow::Result<()> {
        let visibility = Some(visibility.unwrap_or_else(|| vec![public_visibility()]));
        match self.targets.get_mut(&name) {
            Some(Target::SourceFile(file)) => {
                file.visibility = visibility;
                Ok(())
            }
            Some(_) => anyhow::bail!("exports_files: '{name}' is already a rule or output file of this package"),
            None => {
                self.targets.insert(name, Target::SourceFile(SourceFile { name, visibility }));
                Ok(())
            }
        }
    }

    pub fn add_package_group(&mut self, group: PackageGroup) -> anyhow::Result<()> {
        self.check_name_free(group.name, "package group")?;
        self.targets.insert(group.name, Target::PackageGroup(group));
        Ok(())
    }

    /// Finish the package: add a source file target for each file in the
    /// package that a rule refers to, and the BUILD file itself.
    pub fn finish(mut self) -> Package {
        let mut referenced = Vec::new();
        for target in self.targets.values() {
            let Target::Rule(rule) = target else {
                continue;
            };
            for (attr, value) in &rule.attrs {
                if attr.as_str() == "visibility" {
                    continue;
                }
                value.labels(&mut referenced);
            }
        }
        referenced.push(self.id.label(self.build_file_name));
        for label in referenced {
            if label.package != self.id || self.targets.contains_key(&label.name) {
                continue;
            }
            let file = SourceFile {
                name: label.name,
                visibility: None,
            };
            self.targets.insert(label.name, Target::SourceFile(file));
        }

        Package {
            id: self.id,
            build_file: self.id.label(self.build_file_name),
            rule_classes: self.rule_classes,
            targets: self.targets.into_values().collect(),
            loads: self.loads,
            args: self.args,
            errors: self.errors,
        }
    }

    fn check_name_free(&self, name: Symbol, what: &str) -> anyhow::Result<()> {
        let Some(existing) = self.targets.get(&name) else {
            return Ok(());
        };
        let existing = match existing {
            Target::Rule(_) => "rule",
            Target::SourceFile(_) => "source file",
            Target::OutputFile { .. } => "output file",
            Target::PackageGroup(_) => "package group",
        };
        anyhow::bail!("{what} '{name}' in package '{}' conflicts with existing {existing}", self.id.path)
    }

    fn intern_class(&mut self, class: RuleClassRef) -> u32 {
        if let Some(&index) = self.rule_class_index.get(&class) {
            return index;
        }
        let index = self.rule_classes.len() as u32;
        self.rule_classes.push(class.clone());
        self.rule_class_index.insert(class, index);
        index
    }
}

/// `//visibility:public`.
pub fn public_visibility() -> Label {
    Label::parse_canonical("//visibility:public").expect("//visibility:public is a valid label")
}
