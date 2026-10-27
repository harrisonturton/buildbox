//! `runfiles`: the files an executable needs when it runs.

use crate::file_value::FileValue;
use allocative::Allocative;
use bazel_starlark::depset::{Depset, Order};
use starlark::coerce::Coerce;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::structs::AllocStruct;
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
    /// Whether the tree gets empty `__init__.py` files wherever Python
    /// needs them to import its files, as Python rules ask.
    #[freeze(identity)]
    pub init_py_files: bool,
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
            init_py_files: false,
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
            merged.init_py_files |= runfiles.init_py_files;
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

impl<'v, V: ValueLike<'v>> RunfilesGen<V> {
    /// The empty files the tree gets, at their paths relative to its root,
    /// sorted: the `__init__.py` files Python needs, if it asks for them.
    pub fn empty_files(&self, workspace_dir: &str) -> Vec<String> {
        let mut paths: Vec<String> = self
            .empty_filenames()
            .into_iter()
            .map(|path| match path.strip_prefix("../") {
                Some(external) => external.to_string(),
                None => format!("{workspace_dir}/{path}"),
            })
            .collect();
        paths.sort();
        paths
    }

    /// The empty files the tree gets, as Bazel's `empty_filenames` gives
    /// them: relative to the main repository's directory, with those of
    /// other repositories under `../`.
    fn empty_filenames(&self) -> Vec<String> {
        if !self.init_py_files {
            return Vec::new();
        }
        let mut paths: Vec<String> = self.symlinks.iter().map(|(path, _)| path.clone()).collect();
        if let Some(files) = Depset::from_value(self.files.to_value()) {
            paths.extend(files.to_list().into_iter().filter_map(FileValue::artifact).map(|artifact| artifact.short_path()));
        }
        init_py_files(&paths)
    }
}

/// The `__init__.py` files Python needs to import the `.py`, `.pyc` and
/// `.so` files of a tree whose files are at `paths`, as Bazel's
/// `GetInitPyFiles` finds them: one in each directory above such a file,
/// except `__pycache__` directories and those that have an `__init__.py`
/// or `__init__.pyc`. They're in the order Bazel's Java `HashSet` of them
/// iterates in, which `empty_filenames` gives.
pub fn init_py_files(paths: &[String]) -> Vec<String> {
    let present: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut needed = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if ![".py", ".pyc", ".so"].iter().any(|extension| path.ends_with(extension)) {
            continue;
        }
        let mut dir = path.as_str();
        while let Some((parent, _)) = dir.rsplit_once('/') {
            dir = parent;
            if dir == "__pycache__" || dir.ends_with("/__pycache__") {
                continue;
            }
            let init_py = format!("{dir}/__init__.py");
            let init_pyc = format!("{dir}/__init__.pyc");
            if !present.contains(init_py.as_str()) && !present.contains(init_pyc.as_str()) && seen.insert(init_py.clone()) {
                needed.push(init_py);
            }
        }
    }
    java_hash_set_order(needed)
}

/// Strings, given in the order they were added to a Java `HashSet`, in
/// the order it iterates them: by bucket of their spread `String` hash in
/// the table the set grew to, then in the order they were added.
fn java_hash_set_order(added: Vec<String>) -> Vec<String> {
    let mut capacity: u32 = 16;
    while added.len() as u32 > capacity / 4 * 3 {
        capacity *= 2;
    }
    let bucket = |text: &str| {
        let hash = text.encode_utf16().fold(0u32, |hash, unit| hash.wrapping_mul(31).wrapping_add(u32::from(unit)));
        (hash ^ (hash >> 16)) & (capacity - 1)
    };
    let mut ordered: Vec<(u32, String)> = added.into_iter().map(|text| (bucket(&text), text)).collect();
    ordered.sort_by_key(|(bucket, _)| *bucket);
    ordered.into_iter().map(|(_, text)| text).collect()
}

/// Symlinks as `runfiles.symlinks` lists them: a depset of entries with
/// their `path` and `target_file`.
fn symlink_entries<'v, V: ValueLike<'v>>(symlinks: &[(String, V)], heap: Heap<'v>) -> Value<'v> {
    let entries = symlinks
        .iter()
        .map(|(path, file)| heap.alloc(AllocStruct([("path", heap.alloc(path.as_str())), ("target_file", file.to_value())])))
        .collect();
    heap.alloc(Depset {
        direct: entries,
        transitive: Vec::new(),
        order: Order::Default,
    })
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
            "symlinks" => Some(symlink_entries(&self.symlinks, heap)),
            "root_symlinks" => Some(symlink_entries(&self.root_symlinks, heap)),
            "empty_filenames" => {
                let names: Vec<Value<'v>> = self.empty_filenames().iter().map(|name| heap.alloc(name.as_str())).collect();
                Some(heap.alloc(Depset {
                    direct: names,
                    transitive: Vec::new(),
                    order: Order::Default,
                }))
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_gets_init_files_where_bazel_adds_them() {
        let paths = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<Vec<_>>();
        let files = paths(&["a/b/c.py", "a/__init__.py", "a/__pycache__/x.pyc", "../repo/m.so", "data/x.txt"]);
        let mut found = init_py_files(&files);
        found.sort();
        assert_eq!(found, paths(&["../__init__.py", "../repo/__init__.py", "a/b/__init__.py"]));
    }

    #[test]
    fn hash_sets_iterate_as_java_does() {
        // Java's `new HashSet<>(List.of("b", "a", "c", "bb"))` iterates as
        // bb, a, b, c: "bb" hashes to 3136, in bucket 0 of 16, and one-letter
        // strings hash to their character.
        let strings = |items: &[&str]| items.iter().map(|item| item.to_string()).collect::<Vec<_>>();
        assert_eq!(java_hash_set_order(strings(&["b", "a", "c", "bb"])), strings(&["bb", "a", "b", "c"]));
    }
}
