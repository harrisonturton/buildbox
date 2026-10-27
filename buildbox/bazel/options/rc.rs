//! `.bazelrc` files.

use crate::Arg;
use std::path::{Path, PathBuf};

/// Options from `.bazelrc` files, by command and config: `build` lines are
/// under (`build`, `None`), `build:opt` lines under (`build`, `opt`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RcFile {
    entries: Vec<(String, Option<String>, Vec<String>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RcError {
    #[error("failed to read {}: {error}", path.display())]
    Read { path: PathBuf, error: String },

    #[error("{}:{line}: import of {} is recursive", path.display(), import.display())]
    RecursiveImport { path: PathBuf, line: usize, import: PathBuf },
}

impl RcFile {
    /// The options given for `command` without a config.
    pub fn command_args(&self, command: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(entry, config, _)| entry == command && config.is_none())
            .flat_map(|(_, _, args)| args.iter().cloned())
            .collect()
    }

    /// The options `--config=config` expands to for `command`: those under
    /// `common:config` and the configs of the commands it inherits from.
    /// `None` if no file defines the config at all.
    pub fn config_args(&self, command: &str, config: &str) -> Option<Vec<Arg>> {
        let mut commands = vec!["always", "common"];
        commands.extend(crate::inherited_commands(command));
        let defined = self.entries.iter().any(|(_, entry_config, _)| entry_config.as_deref() == Some(config));
        if !defined {
            return None;
        }
        let mut args = Vec::new();
        for inherited in commands {
            for (entry, entry_config, entry_args) in &self.entries {
                if entry == inherited && entry_config.as_deref() == Some(config) {
                    args.extend(entry_args.iter().map(|arg| Arg { text: arg.clone(), common: inherited == "common" }));
                }
            }
        }
        Some(args)
    }

    /// Parse one file's text. `import` lines are resolved by `read`, which
    /// returns `None` for a missing file.
    fn parse_text(
        &mut self,
        path: &Path,
        text: &str,
        workspace: &Path,
        read: &dyn Fn(&Path) -> Result<Option<String>, RcError>,
        importing: &mut Vec<PathBuf>,
    ) -> Result<(), RcError> {
        let joined = text.replace("\\\n", " ");
        for (index, line) in joined.lines().enumerate() {
            let words = split_words(line);
            let Some((command, rest)) = words.split_first() else {
                continue;
            };
            if command == "import" || command == "try-import" {
                let Some(target) = rest.first() else {
                    continue;
                };
                let target = PathBuf::from(target.replace("%workspace%", &workspace.display().to_string()));
                if importing.contains(&target) {
                    return Err(RcError::RecursiveImport {
                        path: path.to_path_buf(),
                        line: index + 1,
                        import: target,
                    });
                }
                match read(&target)? {
                    Some(imported) => {
                        importing.push(target.clone());
                        self.parse_text(&target, &imported, workspace, read, importing)?;
                        importing.pop();
                    }
                    None if command == "import" => {
                        return Err(RcError::Read {
                            path: target,
                            error: "no such file".into(),
                        })
                    }
                    None => {}
                }
                continue;
            }
            let (command, config) = match command.split_once(':') {
                Some((command, config)) => (command.to_string(), Some(config.to_string())),
                None => (command.clone(), None),
            };
            self.entries.push((command, config, rest.to_vec()));
        }
        Ok(())
    }
}

/// Split a line into words as Bazel does: whitespace-separated, with quotes
/// grouping, and `#` starting a comment.
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut has_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '#') => break,
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                has_word = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    has_word = true;
                }
            }
            (_, c) => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(current);
    }
    words
}

/// Read the `.bazelrc` files Bazel reads: the system one, the workspace's,
/// the user's, and any given with `--bazelrc`, unless `--ignore_all_rc_files`.
pub fn parse_rc_files(workspace: &Path, home: Option<&Path>, explicit: &[PathBuf], ignore_all: bool) -> Result<RcFile, RcError> {
    let mut rc = RcFile::default();
    if ignore_all {
        return Ok(rc);
    }
    let read = |path: &Path| -> Result<Option<String>, RcError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RcError::Read {
                path: path.to_path_buf(),
                error: error.to_string(),
            }),
        }
    };
    let mut files = vec![PathBuf::from("/etc/bazel.bazelrc"), workspace.join(".bazelrc")];
    if let Some(home) = home {
        files.push(home.join(".bazelrc"));
    }
    files.extend(explicit.iter().cloned());
    for file in files {
        if let Some(text) = read(&file)? {
            let mut importing = vec![file.clone()];
            rc.parse_text(&file, &text, workspace, &read, &mut importing)?;
        }
    }
    Ok(rc)
}

#[cfg(test)]
pub(crate) fn parse_for_test(text: &str) -> RcFile {
    let mut rc = RcFile::default();
    let read = |_: &Path| -> Result<Option<String>, RcError> { Ok(None) };
    rc.parse_text(Path::new("/w/.bazelrc"), text, Path::new("/w"), &read, &mut Vec::new())
        .unwrap();
    rc
}
