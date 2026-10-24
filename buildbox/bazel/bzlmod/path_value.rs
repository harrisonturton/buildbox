//! Starlark's `path` type, which repository and module contexts return.

use allocative::Allocative;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::list::AllocList;
use starlark::values::none::NoneOr;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;
use std::path::PathBuf;

/// A path on this machine. `str()` gives it as an absolute path.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct PathValue(#[allocative(skip)] pub PathBuf);

starlark_simple_value!(PathValue);

impl fmt::Display for PathValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[starlark_value(type = "path")]
impl<'v> StarlarkValue<'v> for PathValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("path", path_methods);
        Some(METHODS.methods())
    }

    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&self.0.display().to_string());
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<PathValue>().is_some_and(|other| other.0 == self.0))
    }
}

#[starlark_module]
fn path_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn basename(this: &PathValue) -> anyhow::Result<String> {
        Ok(this.0.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default())
    }

    #[starlark(attribute)]
    fn dirname(this: &PathValue) -> anyhow::Result<NoneOr<PathValue>> {
        Ok(match this.0.parent() {
            Some(parent) => NoneOr::Other(PathValue(parent.to_path_buf())),
            None => NoneOr::None,
        })
    }

    #[starlark(attribute)]
    fn exists(this: &PathValue) -> anyhow::Result<bool> {
        Ok(this.0.exists())
    }

    #[starlark(attribute)]
    fn is_dir(this: &PathValue) -> anyhow::Result<bool> {
        Ok(this.0.is_dir())
    }

    #[starlark(attribute)]
    fn realpath(this: &PathValue) -> anyhow::Result<PathValue> {
        Ok(PathValue(std::fs::canonicalize(&this.0).unwrap_or_else(|_| this.0.clone())))
    }

    fn get_child(this: &PathValue, #[starlark(args)] names: UnpackTuple<&str>) -> anyhow::Result<PathValue> {
        let mut path = this.0.clone();
        for name in names.items {
            path.push(name);
        }
        Ok(PathValue(path))
    }

    fn readdir<'v>(this: &PathValue, #[starlark(require = named)] watch: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = watch;
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&this.0)
            .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", this.0.display()))?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        Ok(heap.alloc(AllocList(entries.into_iter().map(PathValue))))
    }
}
