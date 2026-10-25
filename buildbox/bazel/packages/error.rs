//! Errors from evaluating BUILD files, kept with where they happened so
//! that commands can report them as Bazel does.
//!
//! Files are named by their labels, as evaluation names them; a command
//! names them by their paths when it reports an error.

use skycodec::{Decode, Encode};
use std::fmt;

/// An error evaluating a BUILD file.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PackageError {
    /// Where an error found at a single place is: a syntax or name error,
    /// or an invalid rule.
    pub position: Option<SourcePosition>,
    /// The calls a failed evaluation was in, outermost first. An error
    /// with a traceback has no position.
    pub traceback: Vec<TraceFrame>,
    /// What went wrong, as Bazel words it: after the position, or as the
    /// traceback's last line.
    pub message: String,
}

/// A line and column in a file.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SourcePosition {
    /// The file's label.
    pub file: String,
    /// One-based.
    pub line: u32,
    /// One-based.
    pub column: u32,
}

/// One call of a traceback: where it was made, in which function, and the
/// line of source making it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TraceFrame {
    pub position: SourcePosition,
    /// The function making the call, or `<toplevel>`.
    pub function: String,
    pub source: String,
}

impl PackageError {
    /// An error with neither a position nor a traceback.
    pub fn new(message: impl Into<String>) -> PackageError {
        PackageError {
            position: None,
            traceback: Vec::new(),
            message: message.into(),
        }
    }

    /// An error at `position`.
    pub fn at(position: SourcePosition, message: impl Into<String>) -> PackageError {
        PackageError {
            position: Some(position),
            traceback: Vec::new(),
            message: message.into(),
        }
    }

    /// The files the error names, by their labels.
    pub fn files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = self.traceback.iter().map(|frame| frame.position.file.as_str()).collect();
        files.extend(self.position.iter().map(|position| position.file.as_str()));
        files.sort();
        files.dedup();
        files
    }

    /// The error as Bazel reports it when it happens, with files named by
    /// `path_of`: after its position, or with its traceback.
    pub fn report(&self, path_of: &dyn Fn(&str) -> String) -> String {
        match &self.position {
            Some(position) => format!("{}:{}:{}: {}", path_of(&position.file), position.line, position.column, self.message),
            None => self.describe(path_of),
        }
    }

    /// The error as Bazel quotes it in a summary such as "package contains
    /// errors": without its position, but with its traceback.
    pub fn describe(&self, path_of: &dyn Fn(&str) -> String) -> String {
        if self.traceback.is_empty() {
            return self.message.clone();
        }
        let mut text = String::from("Traceback (most recent call last):\n");
        for frame in &self.traceback {
            let position = &frame.position;
            text.push_str(&format!(
                "\tFile \"{}\", line {}, column {}, in {}\n\t\t{}\n",
                path_of(&position.file),
                position.line,
                position.column,
                frame.function,
                frame.source
            ));
        }
        text.push_str(&self.message);
        text
    }
}

/// The error as reported, with files named by their labels.
impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report(&|file| file.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(file: &str, line: u32, column: u32) -> SourcePosition {
        SourcePosition {
            file: file.to_string(),
            line,
            column,
        }
    }

    #[test]
    fn positioned_errors_are_reported_after_their_position() {
        let error = PackageError::at(position("//pkg:BUILD", 2, 1), "name 'foo' is not defined");
        assert_eq!(error.report(&|file| format!("/ws/{file}")), "/ws///pkg:BUILD:2:1: name 'foo' is not defined");
        assert_eq!(error.describe(&|file| file.to_string()), "name 'foo' is not defined");
    }

    #[test]
    fn failures_are_reported_with_their_traceback() {
        let error = PackageError {
            position: None,
            traceback: vec![TraceFrame {
                position: position("//rt:BUILD", 2, 5),
                function: "<toplevel>".to_string(),
                source: "fail(\"boom\")".to_string(),
            }],
            message: "Error in fail: boom".to_string(),
        };
        let expected = "Traceback (most recent call last):\n\tFile \"/rt/BUILD\", line 2, column 5, in <toplevel>\n\t\tfail(\"boom\")\nError in fail: boom";
        assert_eq!(error.report(&|_| "/rt/BUILD".to_string()), expected);
    }
}
