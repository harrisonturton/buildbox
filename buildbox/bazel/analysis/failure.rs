//! How a rule's or an aspect's implementation failed, as Bazel reports it.

use bazel_label::Label;
use bazel_packages::PackageError;
use skyframe::Ctx;
use std::collections::HashMap;

/// An implementation that failed, or returned what isn't providers: what
/// Bazel reports after `in <kind> rule <label>: `.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct ImplementationFailed {
    pub error: PackageError,
    /// Whether Bazel starts the error on a line of its own, as it does for
    /// errors Starlark raises.
    pub own_line: bool,
}

impl ImplementationFailed {
    /// An error the implementation's Starlark raised, with its traceback.
    pub fn starlark(error: &starlark::Error) -> ImplementationFailed {
        ImplementationFailed {
            error: bazel_starlark::reported::package_error(error, &[]),
            own_line: true,
        }
    }

    /// An error Starlark raises on the implementation's behalf, without a
    /// traceback, such as for a value it returned.
    pub fn raised(message: String) -> ImplementationFailed {
        ImplementationFailed {
            error: PackageError::new(message),
            own_line: true,
        }
    }

    /// An error Bazel reports itself, on the line it reports the target
    /// on.
    pub fn reported(message: String) -> ImplementationFailed {
        ImplementationFailed {
            error: PackageError::new(message),
            own_line: false,
        }
    }

    /// The error as Bazel reports it after `in <target>: `, with files
    /// named by their paths.
    pub async fn describe(&self, ctx: &Ctx) -> String {
        let paths = paths(ctx, &self.error).await;
        let described = self.error.describe(&|file| paths.get(file).cloned().unwrap_or_else(|| file.to_string()));
        match self.own_line {
            true => format!("\n{described}"),
            false => described,
        }
    }
}

/// The providers of a target and of the aspects applied to it clash.
#[derive(Debug, thiserror::Error)]
#[error("Provider {0} provided twice")]
pub struct ProvidersClash(pub String);

/// The paths of the source files `error` names, by their labels.
async fn paths(ctx: &Ctx, error: &PackageError) -> HashMap<String, String> {
    let mut paths = HashMap::new();
    for file in error.files() {
        let Ok(label) = Label::parse_canonical(file) else {
            continue;
        };
        if let Some(path) = bazel_loading::source_path(ctx, label).await {
            paths.insert(file.to_string(), path.display().to_string());
        }
    }
    paths
}
