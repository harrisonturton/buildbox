//! A rule's make variables, such as `$(BINDIR)`, from the places Bazel
//! finds them, for `$(...)` in attributes and for `ctx.var`.

use bazel_config::{BuildConfiguration, OptionValue};
use std::collections::BTreeMap;

/// A rule's make variables, by where they come from.
pub struct MakeVariables {
    /// The `--define`s, each name once.
    pub defines: Vec<(String, String)>,
    /// The variables of each `TemplateVariableInfo` the rule sees: those of
    /// the targets its `toolchains` attribute names, in order, then those
    /// of the toolchains it resolves.
    pub templates: Vec<Vec<(String, String)>>,
    /// The configuration's own: `TARGET_CPU`, `COMPILATION_MODE`, `BINDIR`
    /// and `GENDIR`.
    pub global: Vec<(String, String)>,
}

impl MakeVariables {
    /// The make variables of a rule analyzed in `config` for a platform
    /// whose CPU is `target_cpu`, with the variables of the
    /// `TemplateVariableInfo`s it sees.
    pub fn new(config: &BuildConfiguration, target_cpu: &str, templates: Vec<Vec<(String, String)>>) -> MakeVariables {
        let bin_dir = config.bin_dir();
        let global = vec![
            ("TARGET_CPU".to_string(), target_cpu.to_string()),
            ("COMPILATION_MODE".to_string(), config.compilation_mode.clone()),
            ("BINDIR".to_string(), bin_dir.clone()),
            ("GENDIR".to_string(), bin_dir),
        ];
        let defines = match config.options.get("define") {
            Ok(OptionValue::Assignments(defines)) => defines.clone(),
            _ => Vec::new(),
        };
        MakeVariables { defines, templates, global }
    }

    /// What `$(name)` expands to: its value in the first place that defines
    /// it, looking at the `--define`s, then each template in order, then
    /// the configuration's own.
    pub fn lookup(&self, name: &str) -> Option<&str> {
        let places = std::iter::once(&self.defines).chain(&self.templates).chain(std::iter::once(&self.global));
        for variables in places {
            if let Some((_, value)) = variables.iter().find(|(variable, _)| variable == name) {
                return Some(value);
            }
        }
        None
    }

    /// Every variable, as `ctx.var` lists them: the configuration's own,
    /// then the templates' by name, then the `--define`s. A name keeps its
    /// first place and takes its last value, so among templates it takes
    /// the value of the last that defines it, where [`lookup`] takes the
    /// first's, as in Bazel.
    ///
    /// [`lookup`]: MakeVariables::lookup
    pub fn all(&self) -> Vec<(&str, &str)> {
        let mut templates: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, value) in self.templates.iter().flatten() {
            templates.insert(name, value);
        }
        let global = self.global.iter().map(|(name, value)| (name.as_str(), value.as_str()));
        let defines = self.defines.iter().map(|(name, value)| (name.as_str(), value.as_str()));
        let mut all: Vec<(&str, &str)> = Vec::new();
        for (name, value) in global.chain(templates).chain(defines) {
            match all.iter_mut().find(|(existing, _)| *existing == name) {
                Some(entry) => entry.1 = value,
                None => all.push((name, value)),
            }
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    fn example() -> MakeVariables {
        MakeVariables {
            defines: variables(&[("CC", "defined")]),
            templates: vec![
                variables(&[("WORKSPACE_ROOT", "external/llvm+"), ("BINDIR", "first")]),
                variables(&[("CC", "clang"), ("BINDIR", "second"), ("AR", "ar")]),
            ],
            global: variables(&[("TARGET_CPU", "k8"), ("COMPILATION_MODE", "fastbuild"), ("BINDIR", "bazel-out/k8-fastbuild/bin")]),
        }
    }

    #[test]
    fn lookup_takes_defines_then_templates_in_order_then_the_configurations() {
        let make = example();
        assert_eq!(make.lookup("CC"), Some("defined"));
        assert_eq!(make.lookup("BINDIR"), Some("first"));
        assert_eq!(make.lookup("WORKSPACE_ROOT"), Some("external/llvm+"));
        assert_eq!(make.lookup("TARGET_CPU"), Some("k8"));
        assert_eq!(make.lookup("GENDIR"), None);
    }

    #[test]
    fn ctx_var_lists_the_configurations_then_templates_by_name_then_defines() {
        let make = example();
        assert_eq!(
            make.all(),
            [
                ("TARGET_CPU", "k8"),
                ("COMPILATION_MODE", "fastbuild"),
                ("BINDIR", "second"),
                ("AR", "ar"),
                ("CC", "defined"),
                ("WORKSPACE_ROOT", "external/llvm+"),
            ]
        );
    }
}
