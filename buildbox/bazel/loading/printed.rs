//! What Starlark code printed, shown to the user as Bazel's `DEBUG:`
//! messages.

use crate::RepoRootKey;
use bazel_fs::RootedPath;
use bazel_label::Label;
use bazel_progress::{Event, Progress};
use bazel_starlark::print::Printed;
use intern::PathId;
use skyframe::Ctx;
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

/// Where a source file is on disk, as messages name it, if it's known.
pub async fn source_path(ctx: &Ctx, label: Label) -> Option<PathBuf> {
    let root = ctx.compute(&RepoRootKey(label.repo())).await.ok()?;
    let name = PathId::parse(label.name.as_str()).ok()?;
    RootedPath::new(root.0, label.package.path.join_path(name)).resolve(ctx).ok()
}
