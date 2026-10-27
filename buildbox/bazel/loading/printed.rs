//! What Starlark code printed, shown to the user as Bazel's `DEBUG:`
//! messages.

use crate::RepoRootKey;
use bazel_fs::RootedPath;
use bazel_label::Label;
use bazel_packages::PackageError;
use bazel_progress::{Event, Progress};
use bazel_starlark::print::Printed;
use intern::PathId;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::HashMap;
use std::path::PathBuf;

/// Show what was printed to the user, at each file's path on disk.
pub async fn report_printed(ctx: &Ctx, printed: &[Printed]) {
    let Ok(progress) = ctx.global::<Progress>() else {
        return;
    };
    for line in printed {
        let path = match line.label() {
            Some(label) => source_path(ctx, label).await,
            None => None,
        };
        progress.report(Event::Debug(line.message(path.as_deref())));
    }
}

/// A source file's text, read through the graph, if it exists.
pub async fn read_source(ctx: &Ctx, label: Label) -> Option<String> {
    let root = ctx.compute(&RepoRootKey(label.repo())).await.ok()?;
    let name = PathId::parse(label.name.as_str()).ok()?;
    let bytes = bazel_fs::read_file(ctx, RootedPath::new(root.0, label.package.path.join_path(name))).await.ok()??;
    String::from_utf8(bytes).ok()
}

/// A source file's text, if it exists and is UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SourceText(pub Option<String>);

encoded_value!(SourceText);

/// The text of the source file a label names, as [`read_source`] reads
/// it, kept in the graph for those who read it more than once.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SourceTextKey(pub Label);

impl Key for SourceTextKey {
    type Value = SourceText;
    const KIND: &'static str = "SOURCE_TEXT";

    async fn compute(&self, ctx: &Ctx) -> Result<SourceText, SkyError> {
        Ok(SourceText(read_source(ctx, self.0).await))
    }
}

/// Where a source file is on disk, as messages name it, if it's known.
pub async fn source_path(ctx: &Ctx, label: Label) -> Option<PathBuf> {
    let root = ctx.compute(&RepoRootKey(label.repo())).await.ok()?;
    let name = PathId::parse(label.name.as_str()).ok()?;
    RootedPath::new(root.0, label.package.path.join_path(name)).resolve(ctx).ok()
}

/// The paths of the source files an error names, by their labels, as
/// messages name them.
pub async fn error_paths(ctx: &Ctx, error: &PackageError) -> HashMap<String, String> {
    let mut paths = HashMap::new();
    for file in error.files() {
        let Ok(label) = Label::parse_canonical(file) else {
            continue;
        };
        if let Some(path) = source_path(ctx, label).await {
            paths.insert(file.to_string(), path.display().to_string());
        }
    }
    paths
}
