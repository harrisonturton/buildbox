//! `coverage_common`, which rules use to say which of their files coverage
//! instruments.
//!
//! bb doesn't collect coverage yet, so, as Bazel does when coverage is off,
//! every rule instruments nothing.

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::{Heap, Value};

/// Add `coverage_common` to `.bzl` files.
pub fn register(builder: &mut GlobalsBuilder) {
    builder.namespace("coverage_common", coverage_common_functions);
}

#[starlark_module]
fn coverage_common_functions(builder: &mut GlobalsBuilder) {
    /// `instrumented_files_info(ctx, *, source_attributes = [],
    /// dependency_attributes = [], coverage_support_files = [],
    /// coverage_environment = {}, extensions = None, metadata_files = [],
    /// reported_to_actual_sources = None, baseline_coverage_files = None)`:
    /// an `InstrumentedFilesInfo` with no files, since coverage is off.
    fn instrumented_files_info<'v>(
        ctx: Value<'v>,
        #[starlark(require = named)] source_attributes: Option<Value<'v>>,
        #[starlark(require = named)] dependency_attributes: Option<Value<'v>>,
        #[starlark(require = named)] coverage_support_files: Option<Value<'v>>,
        #[starlark(require = named)] coverage_environment: Option<Value<'v>>,
        #[starlark(require = named)] extensions: Option<Value<'v>>,
        #[starlark(require = named)] metadata_files: Option<Value<'v>>,
        #[starlark(require = named)] reported_to_actual_sources: Option<Value<'v>>,
        #[starlark(require = named)] baseline_coverage_files: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (ctx, source_attributes, dependency_attributes, coverage_support_files, coverage_environment);
        let _ = (extensions, metadata_files, reported_to_actual_sources, baseline_coverage_files);
        let fields = vec![("instrumented_files", crate::providers::file_depset(&[], heap)), ("metadata_files", crate::providers::file_depset(&[], heap))];
        crate::providers::instance("InstrumentedFilesInfo", fields, heap)
    }
}
