//! Command lines with directories in them. A directory that an `Args`
//! expands stands for its files, which only exist once the action making
//! the directory has run, so the arguments made from it are expanded then.
//! Until then each directory is its own path, as `aquery` shows it.

use crate::artifact::Artifact;
use intern::PathId;
use skycodec::{Decode, Encode};

/// The arguments one `add_all` or `add_joined` call made from values among
/// which are directories: those from `start`, `len` of them, stand for them
/// until the directories have files.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct DirectoryArgs {
    pub start: u32,
    pub len: u32,
    /// The argument naming the values, if the call gave one.
    pub name: Option<String>,
    pub values: Vec<ArgValue>,
    /// `format_each`, applied to each value and each directory's file.
    pub format_each: Option<String>,
    pub style: ArgStyle,
    pub uniquify: bool,
    pub omit_if_empty: bool,
}

/// A value of a call, as its argument: text, or a directory to expand.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ArgValue {
    Text(String),
    Directory(Artifact),
}

/// How a call writes its values.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ArgStyle {
    /// `add_all`: an argument for each value, each after `before_each`,
    /// and `terminate_with` after them all.
    Each { before_each: Option<String>, terminate_with: Option<String> },
    /// `add_joined`: one argument, the values joined with `join_with`, put
    /// in `format_joined`.
    Joined { join_with: String, format_joined: Option<String> },
}

/// The arguments of a params file, or of a file an `Args` is written to,
/// with the directories among them to expand, and the format to write them
/// in.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WrittenArgs {
    pub arguments: Vec<String>,
    pub directories: Vec<DirectoryArgs>,
    /// `multiline`, `shell` or `gcc_quoted`.
    pub format: String,
}

impl DirectoryArgs {
    /// The directories among the values.
    pub fn directories(&self) -> impl Iterator<Item = Artifact> + '_ {
        self.values.iter().filter_map(|value| match value {
            ArgValue::Directory(directory) => Some(*directory),
            ArgValue::Text(_) => None,
        })
    }

    /// The arguments, with each directory's files, as `files` gives them
    /// for its path relative to the execution root, in its place.
    pub fn expand(&self, files: &dyn Fn(PathId) -> Result<Vec<String>, String>) -> Result<Vec<String>, String> {
        let mut values = Vec::with_capacity(self.values.len());
        for value in &self.values {
            match value {
                ArgValue::Text(text) => values.push(format_each(self.format_each.as_deref(), text)),
                ArgValue::Directory(directory) => values.extend(files(directory.exec_path)?.iter().map(|file| format_each(self.format_each.as_deref(), file))),
            }
        }
        if self.uniquify {
            let mut seen = std::collections::HashSet::new();
            values.retain(|value| seen.insert(value.clone()));
        }
        if values.is_empty() && self.omit_if_empty {
            return Ok(Vec::new());
        }
        let mut arguments: Vec<String> = self.name.iter().cloned().collect();
        match &self.style {
            ArgStyle::Each { before_each, terminate_with } => {
                for value in values {
                    arguments.extend(before_each.iter().cloned());
                    arguments.push(value);
                }
                arguments.extend(terminate_with.iter().cloned());
            }
            ArgStyle::Joined { join_with, format_joined } => {
                arguments.push(format_each(format_joined.as_deref(), &values.join(join_with)));
            }
        }
        Ok(arguments)
    }
}

/// A value in place of a format's `%s`, with `%%` a percent sign. The
/// format was checked when the arguments were first made.
fn format_each(format: Option<&str>, value: &str) -> String {
    let Some(format) = format else {
        return value.to_string();
    };
    let mut out = String::with_capacity(format.len() + value.len());
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('%', Some('%')) => {
                out.push('%');
                chars.next();
            }
            ('%', Some('s')) => {
                out.push_str(value);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// `arguments` with the arguments each of `directories` stands for in
/// place of them, expanded with `files`. The ranges mustn't overlap.
pub fn expand(arguments: &[String], directories: &[DirectoryArgs], files: &dyn Fn(PathId) -> Result<Vec<String>, String>) -> Result<Vec<String>, String> {
    let mut expanded = Vec::with_capacity(arguments.len());
    let mut next = 0;
    let mut sorted: Vec<&DirectoryArgs> = directories.iter().collect();
    sorted.sort_by_key(|directory| directory.start);
    for directory in sorted {
        let start = directory.start as usize;
        let end = start + directory.len as usize;
        if start < next || end > arguments.len() {
            return Err(format!("arguments {start}..{end} made from directories are out of place"));
        }
        expanded.extend_from_slice(&arguments[next..start]);
        expanded.extend(directory.expand(files)?);
        next = end;
    }
    expanded.extend_from_slice(&arguments[next..]);
    Ok(expanded)
}

/// `directories` for arguments that are now `offset` further along.
pub fn shifted(directories: &[DirectoryArgs], offset: usize) -> Vec<DirectoryArgs> {
    directories
        .iter()
        .map(|directory| DirectoryArgs {
            start: directory.start + offset as u32,
            ..directory.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Artifact {
        directory("out/t")
    }

    fn directory(path: &str) -> Artifact {
        Artifact {
            exec_path: PathId::parse(path).unwrap(),
            owner: bazel_label::Label::parse_canonical("//p:t").unwrap(),
            config: None,
            is_directory: true,
        }
    }

    fn files(path: PathId) -> Result<Vec<String>, String> {
        match path.to_string().as_str() {
            "out/t" => Ok(vec!["out/t/a".to_string(), "out/t/c/d".to_string()]),
            _ => Ok(Vec::new()),
        }
    }

    fn each(values: Vec<ArgValue>) -> DirectoryArgs {
        DirectoryArgs {
            start: 1,
            len: 3,
            name: Some("--each".to_string()),
            values,
            format_each: Some("-I%s".to_string()),
            style: ArgStyle::Each {
                before_each: Some("-x".to_string()),
                terminate_with: Some("--end".to_string()),
            },
            uniquify: false,
            omit_if_empty: true,
        }
    }

    #[test]
    fn expands_directories_in_place_of_their_placeholders() {
        let arguments: Vec<String> = ["cmd", "--each", "-x", "-Iout/t", "last"].iter().map(|text| text.to_string()).collect();
        let mut args = each(vec![ArgValue::Directory(dir()), ArgValue::Text("lit".to_string())]);
        args.len = 3;
        let expanded = expand(&arguments, &[args], &files).unwrap();
        assert_eq!(expanded, ["cmd", "--each", "-x", "-Iout/t/a", "-x", "-Iout/t/c/d", "-x", "-Ilit", "--end", "last"]);
    }

    #[test]
    fn joins_and_uniquifies_after_expanding() {
        let args = DirectoryArgs {
            values: vec![ArgValue::Directory(dir()), ArgValue::Directory(dir()), ArgValue::Text("z".to_string())],
            format_each: None,
            style: ArgStyle::Joined {
                join_with: ",".to_string(),
                format_joined: Some("[%s]".to_string()),
            },
            uniquify: true,
            ..each(Vec::new())
        };
        assert_eq!(args.expand(&files).unwrap(), ["--each", "[out/t/a,out/t/c/d,z]"]);
    }

    #[test]
    fn omits_empty_directories_unless_asked_not_to() {
        let empty = directory("out/e");
        let mut args = each(vec![ArgValue::Directory(empty)]);
        assert!(args.expand(&files).unwrap().is_empty());
        args.omit_if_empty = false;
        assert_eq!(args.expand(&files).unwrap(), ["--each", "--end"]);
    }

    #[test]
    fn keeps_escaped_percents_in_formats() {
        assert_eq!(format_each(Some("100%%:%s"), "a"), "100%:a");
    }
}
