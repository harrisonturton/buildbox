//! `print()`: messages for the user, kept with where they were printed so
//! that commands show them as Bazel's `DEBUG:` lines.

use crate::context::EvalContext;
use crate::label_value::LabelValue;
use bazel_label::{Label, RepoMappingTable, RepoName};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::{Value, ValueLike};
use starlark::values::tuple::UnpackTuple;
use starlark::values::none::NoneType;
use std::path::Path;

/// A message `print()` printed, at the `(` of its call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Printed {
    /// The file, as evaluation names it: by its label.
    pub file: String,
    /// One-based.
    pub line: usize,
    /// One-based.
    pub column: usize,
    pub text: String,
}

impl Printed {
    /// The label of the file it was printed in, if it has one.
    pub fn label(&self) -> Option<Label> {
        Label::parse_canonical(&self.file).ok()
    }

    /// The message as Bazel's `DEBUG:` line shows it, after the prefix:
    /// `file:line:column: text`, with the file at `path` if it's known.
    pub fn message(&self, path: Option<&Path>) -> String {
        let file = match path {
            Some(path) => path.display().to_string(),
            None => self.file.clone(),
        };
        format!("{file}:{}:{}: {}", self.line, self.column, self.text)
    }
}

/// A value as `print()` shows it: a label in its shorthand display form,
/// anything else as `str()` gives it.
fn debug_print(value: Value<'_>, main_mapping: Option<&RepoMappingTable>) -> String {
    match value.downcast_ref::<LabelValue>() {
        Some(label) => label.0.shorthand_display_form(main_mapping),
        None => value.to_str(),
    }
}

#[starlark_module]
pub(crate) fn print_function(builder: &mut GlobalsBuilder) {
    /// `print(*args, sep = " ")`: show the arguments to the user, as
    /// `str()` gives them, but with labels as the main repository writes
    /// them.
    fn print<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(require = named, default = " ")] sep: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = EvalContext::get(eval)?;
        let main_mapping = context.mappings.get(&RepoName::MAIN).map(|mapping| &**mapping);
        let text = args.items.iter().map(|arg| debug_print(*arg, main_mapping)).collect::<Vec<_>>().join(sep);
        let Some(span) = eval.call_stack_top_location() else {
            anyhow::bail!("print() was called from outside Starlark code");
        };
        let position = crate::reported::call_position(&span);
        context.printed.borrow_mut().push(Printed {
            file: position.file,
            line: position.line as usize,
            column: position.column as usize,
            text,
        });
        Ok(NoneType)
    }
}
