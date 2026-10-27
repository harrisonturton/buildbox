//! `File`: artifacts as rule implementations see them.

use crate::artifact::Artifact;
use allocative::Allocative;
use bazel_starlark::label_value::LabelValue;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike, starlark_value};
use std::cmp::Ordering;
use std::fmt;
use std::hash::Hash;

/// A file, source or generated, as a Starlark value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct FileValue(#[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")] #[allocative(skip)] pub Artifact);

starlark_simple_value!(FileValue);

impl FileValue {
    pub fn artifact(value: Value<'_>) -> Option<Artifact> {
        value.downcast_ref::<FileValue>().map(|file| file.0)
    }
}

impl fmt::Display for FileValue {
    /// As Bazel prints files: by their paths relative to their roots.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.0.is_source() { "source" } else { "generated" };
        write!(f, "<{kind} file {}>", self.0.root_relative())
    }
}

/// The directory a file is relative to, as `File.root`.
#[derive(Debug, Clone, PartialEq, Eq, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct FileRoot(pub String);

starlark_simple_value!(FileRoot);

impl fmt::Display for FileRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<root {}>", self.0)
    }
}

#[starlark_value(type = "root")]
impl<'v> StarlarkValue<'v> for FileRoot {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "path" => Some(heap.alloc(self.0.as_str())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["path".to_string()]
    }
}

const FILE_ATTRS: &[&str] = &[
    "basename",
    "dirname",
    "extension",
    "is_directory",
    "is_source",
    "is_symlink",
    "owner",
    "path",
    "root",
    "short_path",
    "tree_relative_path",
];

#[starlark_value(type = "File")]
impl<'v> StarlarkValue<'v> for FileValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let artifact = &self.0;
        let path = artifact.exec_path;
        let value = match attribute {
            "path" => heap.alloc(path.to_string()),
            "short_path" => heap.alloc(artifact.short_path()),
            "basename" => heap.alloc(path.name().as_str()),
            "dirname" => heap.alloc(path.parent().map(|parent| parent.to_string()).unwrap_or_default()),
            "extension" => heap.alloc(extension(path.name().as_str())),
            "is_source" => Value::new_bool(artifact.is_source()),
            "is_directory" => Value::new_bool(artifact.is_directory),
            "is_symlink" => Value::new_bool(false),
            "owner" => heap.alloc(LabelValue(artifact.owner)),
            "root" => heap.alloc(FileRoot(artifact.root().to_string())),
            "tree_relative_path" => heap.alloc(""),
            _ => return None,
        };
        Some(value)
    }

    fn dir_attr(&self) -> Vec<String> {
        FILE_ATTRS.iter().map(|name| name.to_string()).collect()
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<FileValue>().is_some_and(|other| other.0 == self.0))
    }

    fn compare(&self, other: Value<'v>) -> starlark::Result<Ordering> {
        match other.downcast_ref::<FileValue>() {
            Some(other) => Ok(self.0.exec_path.to_string().cmp(&other.0.exec_path.to_string())),
            None => starlark::values::ValueError::unsupported_with(self, "compare", other),
        }
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        self.0.hash(hasher);
        Ok(())
    }
}

/// The extension of a file name, without the dot, as `File.extension`.
fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) => &name[index + 1..],
        _ => "",
    }
}
