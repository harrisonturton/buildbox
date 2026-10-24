//! `runfiles`: the files an executable needs when it runs.

use crate::file_value::FileValue;
use allocative::Allocative;
use bazel_starlark::depset::{Depset, Order};
use starlark::coerce::Coerce;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// A set of runfiles: files, and symlinks to files at chosen paths.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct RunfilesGen<V: ValueLifetimeless> {
    /// A depset of files, each at its runfiles path.
    pub files: V,
    /// Files at paths relative to the workspace's directory in the tree.
    pub symlinks: Vec<(String, V)>,
    /// Files at paths relative to the root of the tree.
    pub root_symlinks: Vec<(String, V)>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<RunfilesGen<To>> for RunfilesGen<From> {}

starlark_complex_value!(pub Runfiles);

impl<'v, V: ValueLike<'v>> fmt::Display for RunfilesGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<runfiles>")
    }
}

impl<'v> Runfiles<'v> {
    pub fn new(files: Value<'v>) -> Runfiles<'v> {
        Runfiles {
            files,
            symlinks: Vec::new(),
            root_symlinks: Vec::new(),
        }
    }

    /// No runfiles.
    pub fn empty(heap: Heap<'v>) -> Runfiles<'v> {
        Runfiles::new(empty_depset(heap))
    }

    /// The union of several sets of runfiles.
    pub fn merge_all(all: &[&Runfiles<'v>], heap: Heap<'v>) -> Runfiles<'v> {
        let transitive = all.iter().map(|runfiles| runfiles.files).collect();
        let files = heap.alloc(Depset {
            direct: Vec::new(),
            transitive,
            order: Order::Default,
        });
        let mut merged = Runfiles::new(files);
        for runfiles in all {
            merged.symlinks.extend(runfiles.symlinks.iter().cloned());
            merged.root_symlinks.extend(runfiles.root_symlinks.iter().cloned());
        }
        merged
    }
}

impl<'v, V: ValueLike<'v>> RunfilesGen<V> {
    /// Each file and its path in the tree, relative to the tree's root.
    pub fn entries(&self, workspace_dir: &str) -> Vec<(String, crate::artifact::Artifact)> {
        let mut entries = Vec::new();
        if let Some(files) = Depset::from_value(self.files.to_value()) {
            for file in files.to_list() {
                if let Some(artifact) = FileValue::artifact(file) {
                    entries.push((artifact.runfiles_path(), artifact));
                }
            }
        }
        for (path, file) in &self.symlinks {
            if let Some(artifact) = FileValue::artifact(file.to_value()) {
                entries.push((format!("{workspace_dir}/{path}"), artifact));
            }
        }
        for (path, file) in &self.root_symlinks {
            if let Some(artifact) = FileValue::artifact(file.to_value()) {
                entries.push((path.clone(), artifact));
            }
        }
        entries
    }
}

pub fn empty_depset(heap: Heap<'_>) -> Value<'_> {
    heap.alloc(Depset {
        direct: Vec::new(),
        transitive: Vec::new(),
        order: Order::Default,
    })
}

#[starlark_value(type = "runfiles")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RunfilesGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("runfiles", runfiles_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "files" => Some(self.files.to_value()),
            "symlinks" | "root_symlinks" | "empty_filenames" => Some(empty_depset(heap)),
            _ => None,
        }
    }
}

#[starlark_module]
fn runfiles_methods(builder: &mut MethodsBuilder) {
    /// The union of these runfiles and another set.
    fn merge<'v>(this: Value<'v>, other: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Runfiles<'v>> {
        let this = Runfiles::from_value(this).ok_or_else(|| anyhow::anyhow!("expected runfiles"))?;
        let other = Runfiles::from_value(other).ok_or_else(|| anyhow::anyhow!("merge: expected runfiles, not {}", other.get_type()))?;
        Ok(Runfiles::merge_all(&[this, other], heap))
    }

    /// The union of these runfiles and several other sets.
    fn merge_all<'v>(this: Value<'v>, other: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Runfiles<'v>> {
        let this = Runfiles::from_value(this).ok_or_else(|| anyhow::anyhow!("expected runfiles"))?;
        let others = bazel_starlark::convert::items(other).ok_or_else(|| anyhow::anyhow!("merge_all: expected a list of runfiles"))?;
        let mut all = vec![this];
        for other in others {
            all.push(Runfiles::from_value(other).ok_or_else(|| anyhow::anyhow!("merge_all: expected runfiles, not {}", other.get_type()))?);
        }
        Ok(Runfiles::merge_all(&all, heap))
    }
}

/// What an executable needs to run, as `DefaultInfo.files_to_run`.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct FilesToRunGen<V: ValueLifetimeless> {
    /// The executable `File`, or `None`.
    pub executable: V,
    /// Its `runfiles`.
    pub runfiles: V,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<FilesToRunGen<To>> for FilesToRunGen<From> {}

starlark_complex_value!(pub FilesToRun);

impl<'v, V: ValueLike<'v>> fmt::Display for FilesToRunGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<FilesToRunProvider>")
    }
}

#[starlark_value(type = "FilesToRunProvider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for FilesToRunGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "executable" => Some(self.executable.to_value()),
            "runfiles_manifest" | "repo_mapping_manifest" => Some(Value::new_none()),
            _ => None,
        }
    }
}
