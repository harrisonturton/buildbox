//! `Args`: command lines that rule implementations build up.

use crate::file_value::FileValue;
use allocative::Allocative;
use bazel_starlark::depset::Depset;
use bazel_starlark::label_value::LabelValue;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;
use std::sync::Mutex;

/// A command line. Values are converted to strings as they're added, with
/// `map_each` functions called then.
#[derive(Debug, Default, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Args {
    #[allocative(skip)]
    state: Mutex<ArgsState>,
}

#[derive(Debug, Default, Clone)]
pub struct ArgsState {
    pub arguments: Vec<String>,
    /// The format of the argument that names the params file, such as
    /// `@%s`, if the arguments always go in one.
    pub param_file: Option<String>,
    /// `shell` or `multiline`.
    pub param_file_format: String,
}

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

    fn push(&self, values: impl IntoIterator<Item = String>) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// A value as a command-line argument: a file's path, a label's canonical
/// form, or a string.
pub fn stringify(value: Value<'_>) -> String {
    if let Some(artifact) = FileValue::artifact(value) {
        return artifact.exec_path.to_string();
    }
    if let Some(label) = value.downcast_ref::<LabelValue>() {
        return label.0.canonical_form();
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

/// The settings `add_all` and `add_joined` share.
struct Expansion<'v> {
    map_each: Option<Value<'v>>,
    format_each: Option<&'v str>,
    uniquify: bool,
}

impl<'v> Expansion<'v> {
    fn expand(&self, items: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for item in values(items)? {
            let Some(map_each) = self.map_each else {
                out.push(format(self.format_each, &stringify(item))?);
                continue;
            };
            let mapped = eval.eval_function(map_each, &[item], &[]).map_err(|error| anyhow::anyhow!("{error}"))?;
            if mapped.is_none() {
                continue;
            }
            let mapped = match bazel_starlark::convert::items(mapped) {
                Some(list) => list,
                None => vec![mapped],
            };
            for value in mapped {
                out.push(format(self.format_each, &stringify(value))?);
            }
        }
        if self.uniquify {
            let mut seen = std::collections::HashSet::new();
            out.retain(|value| seen.insert(value.clone()));
        }
        Ok(out)
    }
}

/// The name and values of `add_all` and `add_joined`, which take either
/// `(values)` or `(name, values)`.
fn name_and_values<'v>(first: Value<'v>, values: Option<Value<'v>>) -> (Option<String>, Value<'v>) {
    match values {
        Some(values) => (Some(stringify(first)), values),
        None => (None, first),
    }
}

#[starlark_module]
fn args_methods(builder: &mut MethodsBuilder) {
    /// `add(arg_name_or_value, value = unbound, *, format = None)`.
    fn add<'v>(
        this: Value<'v>,
        arg_name_or_value: Value<'v>,
        value: Option<Value<'v>>,
        #[starlark(require = named)] format: Option<&str>,
    ) -> anyhow::Result<Value<'v>> {
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let mut added = Vec::new();
        let value = match value {
            Some(value) => {
                added.push(stringify(arg_name_or_value));
                value
            }
            None => arg_name_or_value,
        };
        added.push(self::format(format, &stringify(value))?);
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
        let _ = (expand_directories, allow_closure);
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let (name, items) = name_and_values(arg_name_or_values, values);
        let expansion = Expansion {
            map_each: map_each.filter(|value| !value.is_none()),
            format_each,
            uniquify,
        };
        let expanded = expansion.expand(items, eval)?;
        if expanded.is_empty() && omit_if_empty {
            return Ok(this);
        }
        let mut added: Vec<String> = name.into_iter().collect();
        for value in expanded {
            if let Some(before) = before_each {
                added.push(before.to_string());
            }
            added.push(value);
        }
        added.extend(terminate_with.map(str::to_string));
        args.push(added);
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
        let _ = (expand_directories, allow_closure);
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let (name, items) = name_and_values(arg_name_or_values, values);
        let expansion = Expansion {
            map_each: map_each.filter(|value| !value.is_none()),
            format_each,
            uniquify,
        };
        let expanded = expansion.expand(items, eval)?;
        if expanded.is_empty() && omit_if_empty {
            return Ok(this);
        }
        let mut added: Vec<String> = name.into_iter().collect();
        added.push(format(format_joined, &expanded.join(join_with))?);
        args.push(added);
        Ok(this)
    }

    /// `use_param_file(param_file_arg, *, use_always = False)`. The
    /// arguments go in a params file only when `use_always` is set.
    fn use_param_file<'v>(
        this: Value<'v>,
        param_file_arg: &str,
        #[starlark(require = named, default = false)] use_always: bool,
    ) -> anyhow::Result<Value<'v>> {
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        format(Some(param_file_arg), "")?;
        if use_always {
            let param_file_arg = param_file_arg.to_string();
            args.update(|state| state.param_file = Some(param_file_arg));
        }
        Ok(this)
    }

    /// `set_param_file_format(format)`: `shell`, `multiline` or
    /// `flag_per_line`.
    fn set_param_file_format<'v>(this: Value<'v>, format: &str) -> anyhow::Result<Value<'v>> {
        let args = this.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("expected Args"))?;
        let format = format.to_string();
        args.update(|state| state.param_file_format = format);
        Ok(this)
    }
}

/// The contents of a params file holding `arguments`.
pub fn param_file_contents(state: &ArgsState) -> String {
    let lines: Vec<String> = match state.param_file_format.as_str() {
        "shell" => state.arguments.iter().map(|argument| shell_quote(argument)).collect(),
        _ => state.arguments.clone(),
    };
    let mut contents = lines.join("\n");
    contents.push('\n');
    contents
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
        };
        let expanded = expansion.expand(values, eval)?;
        dict.insert(key, format(format_joined, &expanded.join(join_with))?);
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn formats_with_escaped_percents() {
        assert_eq!(format(Some("--x=%s"), "a").unwrap(), "--x=a");
        assert_eq!(format(Some("%%s is %s, 100%%"), "b").unwrap(), "%s is b, 100%");
        assert!(format(Some("%s%s"), "c").is_err());
        assert!(format(Some("%d"), "c").is_err());
    }
}
