//! `Args`: command lines that rule implementations build up.

use crate::command_line::{ArgStyle, ArgValue, DirectoryArgs};
use crate::file_value::FileValue;
use allocative::Allocative;
use bazel_starlark::depset::Depset;
use bazel_starlark::label_value::LabelValue;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use crate::state::RuleState;
use bazel_label::RepoMappingTable;
use std::fmt;
use std::sync::{Arc, Mutex};

/// A command line. Values are converted to strings as they're added, with
/// `map_each` functions called then; directories to expand are recorded,
/// to be expanded when they have files.
#[derive(Debug, Default, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Args {
    #[allocative(skip)]
    state: Mutex<ArgsState>,
}

#[derive(Debug, Default, Clone)]
pub struct ArgsState {
    pub arguments: Vec<String>,
    /// Where each call that added arguments, after the first, started
    /// adding them, which `flag_per_line` joins.
    pub groups: Vec<usize>,
    /// The format of the argument that names the params file, such as
    /// `@%s`, if the arguments go in one.
    pub param_file: Option<String>,
    /// Whether the arguments go in the params file only when the command
    /// line would be too long without it, rather than always.
    pub param_file_when_needed: bool,
    /// `shell`, `multiline`, `flag_per_line` or `gcc_quoted`.
    pub param_file_format: String,
    /// The arguments made from directories, which stand for their files.
    pub directory_args: Vec<DirectoryArgs>,
}

impl ArgsState {
    /// The arguments as an action gets them: with `flag_per_line`, each
    /// call's arguments, if it added more than one, joined into one,
    /// `first=rest`, as Bazel joins them.
    pub fn lines(&self) -> Vec<String> {
        if self.param_file_format != FLAG_PER_LINE {
            return self.arguments.clone();
        }
        let mut bounds = vec![0];
        bounds.extend(self.groups.iter().copied());
        bounds.push(self.arguments.len());
        let mut lines = Vec::new();
        for window in bounds.windows(2) {
            let group = &self.arguments[window[0]..window[1]];
            match group {
                [] => {}
                [single] => lines.push(single.clone()),
                [first, rest @ ..] if first.is_empty() => lines.push(rest.join(" ")),
                [first, rest @ ..] => lines.push(format!("{first}={}", rest.join(" "))),
            }
        }
        lines
    }

    /// Whether only flags go in the params file, the rest staying on the
    /// command line, as for `flag_per_line`.
    pub fn flags_only(&self) -> bool {
        self.param_file_format == FLAG_PER_LINE
    }
}

/// The params file format of one flag, with its value, per line.
const FLAG_PER_LINE: &str = "flag_per_line";

starlark_simple_value!(Args);

impl fmt::Display for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<Args {:?}>", self.state().arguments)
    }
}

impl Args {
    pub fn state(&self) -> ArgsState {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Arguments built natively, such as a link's.
    pub fn from_state(state: ArgsState) -> Args {
        Args {
            state: Mutex::new(state),
        }
    }

    fn push(&self, values: Vec<String>) {
        self.push_expanding(values, None);
    }

    /// Add a call's arguments, which, if it expands directories, stand for
    /// those `directories` describes.
    fn push_expanding(&self, values: Vec<String>, directories: Option<DirectoryArgs>) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = state.arguments.len();
        if start > 0 {
            state.groups.push(start);
        }
        if let Some(mut directories) = directories {
            directories.start = start as u32;
            directories.len = values.len() as u32;
            state.directory_args.push(directories);
        }
        state.arguments.extend(values);
    }

    fn update(&self, change: impl FnOnce(&mut ArgsState)) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        change(&mut state);
    }
}

#[starlark_value(type = "Args")]
impl<'v> StarlarkValue<'v> for Args {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("Args", args_methods);
        Some(METHODS.methods())
    }
}

/// The directory `value` is, if it's a generated one, whose files an
/// `Args` expands.
fn tree_artifact(value: Value<'_>) -> Option<crate::artifact::Artifact> {
    FileValue::artifact(value).filter(|artifact| artifact.is_directory && !artifact.is_source())
}

/// A value as a command-line argument: a file's path, a label's canonical
/// form, or a string.
pub fn stringify(value: Value<'_>) -> String {
    stringify_with(value, None)
}

/// A value as a command-line argument: a file's path, a string, or a
/// label as the main repository would write it, seeing the others as
/// `main_mapping` says; without one, its canonical form.
pub fn stringify_with(value: Value<'_>, main_mapping: Option<&RepoMappingTable>) -> String {
    if let Some(artifact) = FileValue::artifact(value) {
        return artifact.exec_path.to_string();
    }
    if let Some(label) = value.downcast_ref::<LabelValue>() {
        return match main_mapping {
            Some(mapping) => label.0.display_form(Some(mapping)),
            None => label.0.canonical_form(),
        };
    }
    match value.unpack_str() {
        Some(text) => text.to_string(),
        None => value.to_str(),
    }
}

/// The values of a list, tuple or depset.
fn values(value: Value<'_>) -> anyhow::Result<Vec<Value<'_>>> {
    if let Some(depset) = Depset::from_value(value) {
        return Ok(depset.to_list());
    }
    bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("expected a list or depset, not {}", value.get_type()))
}

/// Put `value` in place of a template's `%s`, which it must have once.
/// `%%` stands for a percent sign.
fn format(template: Option<&str>, value: &str) -> anyhow::Result<String> {
    let Some(template) = template else {
        return Ok(value.to_string());
    };
    let mut out = String::with_capacity(template.len() + value.len());
    let mut substitutions = 0;
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('s') => {
                substitutions += 1;
                out.push_str(value);
            }
            _ => anyhow::bail!("format '{template}' has a '%' that isn't '%s' or '%%'"),
        }
    }
    if substitutions != 1 {
        anyhow::bail!("format '{template}' must contain '%s' exactly once");
    }
    Ok(out)
}

/// Whether a `map_each` function takes a directory expander as well as the
/// item, as Bazel decides: by having two parameters or more.
fn wants_directory_expander(function: Value<'_>) -> bool {
    function.parameters_spec().is_some_and(|parameters| parameters.len() >= 2)
}

/// What `map_each` functions are given to expand directories with. bb
/// expands command lines during analysis, before directories have
/// contents, so a directory expands to itself, as with Bazel's expander
/// where expansion isn't possible.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
struct DirectoryExpander;

starlark_simple_value!(DirectoryExpander);

impl fmt::Display for DirectoryExpander {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<DirectoryExpander>")
    }
}

#[starlark_value(type = "DirectoryExpander")]
impl<'v> StarlarkValue<'v> for DirectoryExpander {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("DirectoryExpander", directory_expander_methods);
        Some(METHODS.methods())
    }
}

#[starlark_module]
fn directory_expander_methods(builder: &mut MethodsBuilder) {
    /// The files of `file`, if it's a directory, or else `[file]`. bb
    /// can't list a directory's files when command lines are made.
    fn expand<'v>(this: Value<'v>, #[starlark(require = pos)] file: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        if let Some(directory) = tree_artifact(file) {
            anyhow::bail!("can't expand directory '{}': bb doesn't support expanding directories in map_each functions yet", directory.exec_path);
        }
        Ok(heap.alloc(starlark::values::list::AllocList([file])))
    }
}

/// The settings `add_all` and `add_joined` share.
struct Expansion<'v> {
    map_each: Option<Value<'v>>,
    format_each: Option<&'v str>,
    uniquify: bool,
    expand_directories: bool,
}

/// A call's values as arguments, each directory as its own path, and, if
/// there are directories to expand, the values to expand later.
struct Expanded {
    arguments: Vec<String>,
    directories: Option<Vec<ArgValue>>,
}

impl<'v> Expansion<'v> {
    fn expand(&self, items: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Expanded> {
        let items = values(items)?;
        let directories = match self.expand_directories {
            true => self.directory_values(&items, eval)?,
            false => None,
        };
        let arguments = self.arguments(items, eval)?;
        Ok(Expanded { arguments, directories })
    }

    /// The values, if any is a directory to expand: text, or directories.
    fn directory_values(&self, items: &[Value<'v>], eval: &Evaluator<'v, '_, '_>) -> anyhow::Result<Option<Vec<ArgValue>>> {
        let Some(first) = items.iter().find_map(|item| tree_artifact(*item)) else {
            return Ok(None);
        };
        if self.map_each.is_some() {
            anyhow::bail!("can't expand directory '{}': bb doesn't support expanding directories for map_each functions yet", first.exec_path);
        }
        let main_mapping = main_mapping(eval);
        let values = items
            .iter()
            .map(|item| match tree_artifact(*item) {
                Some(directory) => ArgValue::Directory(directory),
                None => ArgValue::Text(stringify_with(*item, main_mapping.as_deref())),
            })
            .collect();
        Ok(Some(values))
    }

    fn arguments(&self, items: Vec<Value<'v>>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Vec<String>> {
        let main_mapping = main_mapping(eval);
        let mut out = Vec::new();
        for item in items {
            let Some(map_each) = self.map_each else {
                out.push(format(self.format_each, &stringify_with(item, main_mapping.as_deref()))?);
                continue;
            };
            let mapped = match wants_directory_expander(map_each) {
                true => {
                    let expander = eval.heap().alloc(DirectoryExpander);
                    eval.eval_function(map_each, &[item, expander], &[])
                }
                false => eval.eval_function(map_each, &[item], &[]),
            };
            let mapped = mapped.map_err(|error| anyhow::anyhow!("{error}"))?;
            if mapped.is_none() {
                continue;
            }
            let mapped = match bazel_starlark::convert::items(mapped) {
                Some(list) => list,
                None => vec![mapped],
            };
            for value in mapped {
                out.push(format(self.format_each, &stringify_with(value, main_mapping.as_deref()))?);
            }
        }
        if self.uniquify {
            let mut seen = std::collections::HashSet::new();
            out.retain(|value| seen.insert(value.clone()));
        }
        Ok(out)
    }
}

/// What a call that expands directories adds, but for where its arguments
/// go, which [`Args::push_expanding`] sets.
fn directory_args(name: Option<String>, values: Vec<ArgValue>, format_each: Option<&str>, style: ArgStyle, uniquify: bool, omit_if_empty: bool) -> DirectoryArgs {
    DirectoryArgs {
        start: 0,
        len: 0,
        name,
        values,
        format_each: format_each.map(str::to_string),
        style,
        uniquify,
        omit_if_empty,
    }
}

/// The name and values of `add_all` and `add_joined`, which take either
/// `(values)` or `(name, values)`.
fn name_and_values<'v>(first: Value<'v>, values: Option<Value<'v>>, eval: &Evaluator<'v, '_, '_>) -> (Option<String>, Value<'v>) {
    match values {
        Some(values) => (Some(stringify_with(first, main_mapping(eval).as_deref())), values),
        None => (None, first),
    }
}

/// How the main repository of the rule being analyzed sees the others,
/// if a rule is being analyzed.
fn main_mapping(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepoMappingTable>> {
    RuleState::get(eval).ok().map(|state| state.main_mapping.clone())
}

#[starlark_module]
fn args_methods(builder: &mut MethodsBuilder) {
    /// `add(arg_name_or_value, value = unbound, *, format = None)`.
    fn add<'v>(
        this: Value<'v>,
        arg_name_or_value: Value<'v>,
        value: Option<Value<'v>>,
        #[starlark(require = named)] format: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let main_mapping = main_mapping(eval);
        let mut added = Vec::new();
        let value = match value {
            Some(value) => {
                added.push(stringify_with(arg_name_or_value, main_mapping.as_deref()));
                value
            }
            None => arg_name_or_value,
        };
        if tree_artifact(value).is_some() {
            anyhow::bail!(
                "Cannot add directories to Args#add since they may expand to multiple values. Either use Args#add_all (if you want expansion) or args.add(directory.path) (if you do not)."
            );
        }
        added.push(self::format(format, &stringify_with(value, main_mapping.as_deref()))?);
        args.push(added);
        Ok(this)
    }

    /// `add_all(arg_name_or_values, values = unbound, *, map_each = None,
    /// format_each = None, before_each = None, omit_if_empty = True,
    /// uniquify = False, expand_directories = True, terminate_with = None,
    /// allow_closure = False)`.
    fn add_all<'v>(
        this: Value<'v>,
        arg_name_or_values: Value<'v>,
        values: Option<Value<'v>>,
        #[starlark(require = named)] map_each: Option<Value<'v>>,
        #[starlark(require = named)] format_each: Option<&'v str>,
        #[starlark(require = named)] before_each: Option<&str>,
        #[starlark(require = named, default = true)] omit_if_empty: bool,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named, default = true)] expand_directories: bool,
        #[starlark(require = named)] terminate_with: Option<&str>,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = allow_closure;
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let (name, items) = name_and_values(arg_name_or_values, values, eval);
        let expansion = Expansion {
            map_each: map_each.filter(|value| !value.is_none()),
            format_each,
            uniquify,
            expand_directories,
        };
        let expanded = expansion.expand(items, eval)?;
        if expanded.arguments.is_empty() && omit_if_empty {
            return Ok(this);
        }
        let mut added: Vec<String> = name.iter().cloned().collect();
        for value in expanded.arguments {
            if let Some(before) = before_each {
                added.push(before.to_string());
            }
            added.push(value);
        }
        added.extend(terminate_with.map(str::to_string));
        let style = ArgStyle::Each {
            before_each: before_each.map(str::to_string),
            terminate_with: terminate_with.map(str::to_string),
        };
        let directories = expanded.directories.map(|values| directory_args(name, values, format_each, style, uniquify, omit_if_empty));
        args.push_expanding(added, directories);
        Ok(this)
    }

    /// `add_joined(arg_name_or_values, values = unbound, *, join_with,
    /// map_each = None, format_each = None, format_joined = None,
    /// omit_if_empty = True, uniquify = False, expand_directories = True,
    /// allow_closure = False)`.
    fn add_joined<'v>(
        this: Value<'v>,
        arg_name_or_values: Value<'v>,
        values: Option<Value<'v>>,
        #[starlark(require = named)] join_with: &str,
        #[starlark(require = named)] map_each: Option<Value<'v>>,
        #[starlark(require = named)] format_each: Option<&'v str>,
        #[starlark(require = named)] format_joined: Option<&str>,
        #[starlark(require = named, default = true)] omit_if_empty: bool,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named, default = true)] expand_directories: bool,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = allow_closure;
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let (name, items) = name_and_values(arg_name_or_values, values, eval);
        let expansion = Expansion {
            map_each: map_each.filter(|value| !value.is_none()),
            format_each,
            uniquify,
            expand_directories,
        };
        let expanded = expansion.expand(items, eval)?;
        if expanded.arguments.is_empty() && omit_if_empty {
            return Ok(this);
        }
        let mut added: Vec<String> = name.iter().cloned().collect();
        added.push(format(format_joined, &expanded.arguments.join(join_with))?);
        let style = ArgStyle::Joined {
            join_with: join_with.to_string(),
            format_joined: format_joined.map(str::to_string),
        };
        let directories = expanded.directories.map(|values| directory_args(name, values, format_each, style, uniquify, omit_if_empty));
        args.push_expanding(added, directories);
        Ok(this)
    }

    /// `use_param_file(param_file_arg, *, use_always = False)`: the
    /// arguments go in a params file, unless `use_always` is off and the
    /// command line is short enough without one.
    fn use_param_file<'v>(
        this: Value<'v>,
        param_file_arg: &str,
        #[starlark(require = named, default = false)] use_always: bool,
    ) -> anyhow::Result<Value<'v>> {
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        format(Some(param_file_arg), "")?;
        let param_file_arg = param_file_arg.to_string();
        args.update(|state| {
            state.param_file = Some(param_file_arg);
            state.param_file_when_needed = !use_always;
        });
        Ok(this)
    }

    /// `set_param_file_format(format)`: `shell`, `multiline` or
    /// `flag_per_line`.
    fn set_param_file_format<'v>(this: Value<'v>, format: &str) -> anyhow::Result<Value<'v>> {
        if !matches!(format, "shell" | "multiline" | FLAG_PER_LINE) {
            anyhow::bail!("Invalid value for parameter \"format\": Expected one of \"shell\", \"multiline\", \"flag_per_line\"");
        }
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let format = format.to_string();
        args.update(|state| state.param_file_format = format);
        Ok(this)
    }
}

/// The contents of a params file of `format` holding `arguments`.
pub fn param_file_contents(arguments: &[String], format: &str) -> String {
    let lines: Vec<String> = match format {
        "shell" => arguments.iter().map(|argument| shell_quote(argument)).collect(),
        "gcc_quoted" => arguments.iter().map(|argument| gcc_quote(argument)).collect(),
        _ => arguments.to_vec(),
    };
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// An argument as GCC reads it from a params file: special characters
/// escaped with backslashes.
/// The arguments a params file of `format` holds: what
/// [`param_file_contents`] wrote.
pub fn param_file_arguments(contents: &str, format: &str) -> Vec<String> {
    if contents.is_empty() {
        return Vec::new();
    }
    let lines = contents.strip_suffix('\n').unwrap_or(contents).split('\n');
    match format {
        "shell" => lines.map(shell_unquote).collect(),
        "gcc_quoted" => lines.map(gcc_unquote).collect(),
        _ => lines.map(str::to_string).collect(),
    }
}

fn shell_unquote(line: &str) -> String {
    match line.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')) {
        Some(quoted) => quoted.replace("'\\''", "'"),
        None => line.to_string(),
    }
}

fn gcc_unquote(line: &str) -> String {
    if line == "''" {
        return String::new();
    }
    let mut unquoted = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => unquoted.extend(chars.next()),
            character => unquoted.push(character),
        }
    }
    unquoted
}

fn gcc_quote(argument: &str) -> String {
    if argument.is_empty() {
        return "''".to_string();
    }
    let mut quoted = String::with_capacity(argument.len());
    for character in argument.chars() {
        if "'\"\\ \t\r\n\u{c}\u{b}".contains(character) {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

fn shell_quote(argument: &str) -> String {
    let safe = !argument.is_empty()
        && argument.chars().all(|c| c.is_ascii_alphanumeric() || "@%_-+=:,./".contains(c));
    if safe {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}


/// `TemplateDict`: substitutions for `expand_template`, computed as
/// they're added.
#[derive(Debug, Default, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct TemplateDict {
    #[allocative(skip)]
    substitutions: Mutex<Vec<(String, String)>>,
}

starlark_simple_value!(TemplateDict);

impl fmt::Display for TemplateDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<TemplateDict>")
    }
}

impl TemplateDict {
    pub fn substitutions(&self) -> Vec<(String, String)> {
        self.substitutions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn insert(&self, key: &str, value: String) {
        let mut substitutions = self.substitutions.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        substitutions.retain(|(existing, _)| existing != key);
        substitutions.push((key.to_string(), value));
    }
}

#[starlark_value(type = "TemplateDict")]
impl<'v> StarlarkValue<'v> for TemplateDict {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("TemplateDict", template_dict_methods);
        Some(METHODS.methods())
    }
}

#[starlark_module]
fn template_dict_methods(builder: &mut MethodsBuilder) {
    /// `add(key, value)`.
    fn add<'v>(this: Value<'v>, key: &str, value: &str) -> anyhow::Result<Value<'v>> {
        let dict = this.downcast_ref::<TemplateDict>().ok_or_else(|| anyhow::anyhow!("expected TemplateDict"))?;
        dict.insert(key, value.to_string());
        Ok(this)
    }

    /// `add_joined(key, values, *, join_with, map_each, uniquify = False,
    /// format_joined = None, allow_closure = False)`.
    fn add_joined<'v>(
        this: Value<'v>,
        key: &str,
        values: Value<'v>,
        #[starlark(require = named)] join_with: &str,
        #[starlark(require = named)] map_each: Value<'v>,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named)] format_joined: Option<&str>,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = allow_closure;
        let dict = this.downcast_ref::<TemplateDict>().ok_or_else(|| anyhow::anyhow!("expected TemplateDict"))?;
        let expansion = Expansion {
            map_each: Some(map_each).filter(|value| !value.is_none()),
            format_each: None,
            uniquify,
            expand_directories: true,
        };
        let expanded = expansion.expand(values, eval)?;
        if expanded.directories.is_some() {
            anyhow::bail!("bb doesn't support expanding directories in computed substitutions yet");
        }
        dict.insert(key, format(format_joined, &expanded.arguments.join(join_with))?);
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use super::{format, wants_directory_expander};
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::{FrozenHeapName, Value};

    fn eval(code: &str) -> starlark::environment::FrozenModule {
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse("test.bzl", code.to_owned(), &Dialect::Extended).unwrap();
            Evaluator::new(&module).eval_module(ast, &Globals::standard()).unwrap();
            module.freeze_named(FrozenHeapName::User(Box::new("test".to_owned()))).unwrap()
        })
    }

    fn wants_expander(module: &starlark::environment::FrozenModule, name: &str) -> bool {
        wants_directory_expander(module.get(name).unwrap().value())
    }

    #[test]
    fn formats_with_escaped_percents() {
        assert_eq!(format(Some("--x=%s"), "a").unwrap(), "--x=a");
        assert_eq!(format(Some("%%s is %s, 100%%"), "b").unwrap(), "%s is b, 100%");
        assert!(format(Some("%s%s"), "c").is_err());
        assert!(format(Some("%d"), "c").is_err());
    }

    #[test]
    fn gives_a_directory_expander_to_functions_of_two_parameters_or_more() {
        let module = eval("def one(x): pass\ndef two(x, y = 1): pass\ndef named(x, *, y): pass\ndef rest(*args): pass\nshort = lambda x, y: x\n");
        assert!(!wants_expander(&module, "one"));
        assert!(wants_expander(&module, "two"));
        assert!(wants_expander(&module, "named"));
        assert!(!wants_expander(&module, "rest"));
        assert!(wants_expander(&module, "short"));
        assert!(!wants_directory_expander(Value::new_none()));
    }
}

#[cfg(test)]
mod param_file_tests {
    use super::*;

    #[test]
    fn reads_back_what_params_files_hold() {
        let arguments: Vec<String> = ["plain", "with space", "it's", ""].iter().map(|argument| argument.to_string()).collect();
        for format in ["multiline", "shell", "gcc_quoted"] {
            assert_eq!(param_file_arguments(&param_file_contents(&arguments, format), format), arguments, "{format}");
        }
    }

    #[test]
    fn joins_each_calls_arguments_for_flag_per_line() {
        let arguments: Vec<String> = ["--a", "1", "2", "--b", "", "x", "y"].iter().map(|argument| argument.to_string()).collect();
        let state = ArgsState {
            arguments,
            groups: vec![3, 4, 5],
            param_file_format: FLAG_PER_LINE.to_string(),
            ..ArgsState::default()
        };
        assert_eq!(state.lines(), ["--a=1 2", "--b", "", "x=y"]);
    }
}
