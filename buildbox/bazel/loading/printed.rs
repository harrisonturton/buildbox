//! What Starlark code printed, shown to the user as Bazel's `DEBUG:`
//! messages.

use crate::RepoRootKey;
use bazel_fs::RootedPath;
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
        let path = file_path(ctx, line).await;
        progress.report(Event::Debug(line.message(path.as_deref())));
    }
}

/// Where the file something was printed in is on disk, if it's known.
async fn file_path(ctx: &Ctx, printed: &Printed) -> Option<PathBuf> {
    let label = printed.label()?;
    let root = ctx.compute(&RepoRootKey(label.repo())).await.ok()?;
    let name = PathId::parse(label.name.as_str()).ok()?;
    RootedPath::new(root.0, label.package.path.join_path(name)).resolve(ctx).ok()
}
