//! The functions BUILD files call, also reachable from `.bzl` files as
//! `native.*`.

use crate::context::EvalContext;
use crate::convert::{self, LabelParser};
use bazel_packages::{PackageGroup, Target};
use intern::Symbol;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::{StringValue, Value, ValueLike};

#[starlark_module]
pub(crate) fn build_functions(builder: &mut GlobalsBuilder) {
    /// `glob(include, exclude = [], exclude_directories = 1, allow_empty = ...)`.
    fn glob<'v>(
        include: Option<Value<'v>>,
        #[starlark(require = named)] exclude: Option<Value<'v>>,
        #[starlark(require = named, default = 1)] exclude_directories: i32,
        #[starlark(require = named)] allow_empty: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Vec<String>> {
        let context = EvalContext::get(eval)?;
        let include = include.and_then(convert::strings).unwrap_or_default();
        let exclude = exclude.and_then(convert::strings).unwrap_or_default();
        let Some(globber) = &context.globber else {
            anyhow::bail!("glob() can only be used while evaluating a BUILD file");
        };
        globber.glob(&include, &exclude, exclude_directories != 0, allow_empty.unwrap_or(false))
    }

    /// `package(default_visibility = [...], ...)`.
    fn package<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<NoneType> {
        let context = EvalContext::get(eval)?;
        let mut builder = context.package_builder("package()")?;
        if builder.package_called {
            anyhow::bail!("package() can only be called once per BUILD file");
        }
        if builder.has_targets() {
            anyhow::bail!("package() must be called before any rule");
        }
        builder.package_called = true;
        let package = builder.id;
        let mapping = context.mapping_for(package.repo);
        let parser = LabelParser {
            package,
            mapping: &*mapping,
        };
        for (name, value) in kwargs {
            match name.as_str() {
                "default_visibility" => builder.args.default_visibility = labels(value, &parser)?,
                "default_testonly" => builder.args.default_testonly = value.to_bool(),
                "default_deprecation" => builder.args.default_deprecation = value.unpack_str().map(str::to_string),
                "features" => builder.args.features = convert::strings(value).unwrap_or_default(),
                "default_applicable_licenses" => builder.args.default_applicable_licenses = labels(value, &parser)?,
                "default_package_metadata" => builder.args.default_package_metadata = labels(value, &parser)?,
                "licenses" | "distribs" | "default_compatible_with" | "default_restricted_to" | "default_hdrs_check"
                | "default_copts" => {}
                other => anyhow::bail!("package() got an unexpected keyword argument '{other}'"),
            }
        }
        Ok(NoneType)
    }

    /// `exports_files(srcs, visibility = None, licenses = None)`.
    fn exports_files<'v>(
        #[starlark(require = pos)] srcs: Value<'v>,
        visibility: Option<Value<'v>>,
        licenses: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = licenses;
        let context = EvalContext::get(eval)?;
        let mut builder = context.package_builder("exports_files()")?;
        let package = builder.id;
        let mapping = context.mapping_for(package.repo);
        let parser = LabelParser {
            package,
            mapping: &*mapping,
        };
        let visibility = match visibility.filter(|v| !v.is_none()) {
            Some(visibility) => Some(labels(visibility, &parser)?),
            None => None,
        };
        let Some(srcs) = convert::strings(srcs) else {
            anyhow::bail!("exports_files() takes a list of file names");
        };
        for src in srcs {
            let name = convert::target_name(&src)?;
            builder.export_file(name, visibility.clone())?;
        }
        Ok(NoneType)
    }

    /// `licenses(license_types)`: accepted and ignored.
    fn licenses<'v>(#[starlark(require = pos)] license_types: Value<'v>) -> anyhow::Result<NoneType> {
        let _ = license_types;
        Ok(NoneType)
    }

    /// `package_group(name, packages = [], includes = [])`.
    fn package_group<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] packages: Option<Value<'v>>,
        #[starlark(require = named)] includes: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = EvalContext::get(eval)?;
        let mut builder = context.package_builder("package_group()")?;
        let package = builder.id;
        let mapping = context.mapping_for(package.repo);
        let parser = LabelParser {
            package,
            mapping: &*mapping,
        };
        let group = PackageGroup {
            name: convert::target_name(name)?,
            packages: packages.and_then(convert::strings).unwrap_or_default(),
            includes: match includes.filter(|v| !v.is_none()) {
                Some(includes) => labels(includes, &parser)?,
                None => Vec::new(),
            },
        };
        builder.add_package_group(group)?;
        Ok(NoneType)
    }

    /// The path of the package being defined.
    fn package_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        let context = EvalContext::get(eval)?;
        Ok(context.build_package("package_name()")?.path.to_string())
    }

    /// The canonical name of the package's repository, with an `@`.
    fn repository_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        let context = EvalContext::get(eval)?;
        Ok(format!("@{}", context.build_package("repository_name()")?.repo))
    }

    /// The name of the module the package's repository belongs to, or
    /// `None`.
    fn module_name<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let context = EvalContext::get(eval)?;
        Ok(match &context.module {
            Some((name, _)) => eval.heap().alloc(name.as_str()),
            None => Value::new_none(),
        })
    }

    /// The version of the module the package's repository belongs to, or
    /// `None`.
    fn module_version<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let context = EvalContext::get(eval)?;
        Ok(match &context.module {
            Some((_, version)) => eval.heap().alloc(version.as_str()),
            None => Value::new_none(),
        })
    }

    /// The canonical name of the package's repository.
    fn repo_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        let context = EvalContext::get(eval)?;
        Ok(context.build_package("repo_name()")?.repo.to_string())
    }

    /// Like `Label()`, relative to the package being defined.
    fn package_relative_label<'v>(#[starlark(require = pos)] input: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<crate::label_value::LabelValue> {
        if let Some(label) = input.downcast_ref::<crate::label_value::LabelValue>() {
            return Ok(*label);
        }
        let context = EvalContext::get(eval)?;
        let Some(text) = input.unpack_str() else {
            anyhow::bail!("package_relative_label() takes a string or Label");
        };
        Ok(crate::label_value::LabelValue(context.parse_in_package(text)?))
    }

    /// The attributes of a rule already defined in this package, or `None`.
    /// In a module extension, the repositories it has defined so far.
    fn existing_rule<'v>(#[starlark(require = pos)] name: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let context = EvalContext::get(eval)?;
        if let Some(repos) = &context.repos {
            let defined = repos.borrow().iter().any(|repo| repo.name == name);
            if !defined {
                return Ok(Value::new_none());
            }
            let heap = eval.heap();
            return Ok(heap.alloc(AllocDict([("name", heap.alloc(name))])));
        }
        let builder = context.package_builder("existing_rule()")?;
        let Some(Target::Rule(rule)) = builder.target(Symbol::intern(name)) else {
            return Ok(Value::new_none());
        };
        Ok(rule_dict(&builder, rule, eval))
    }

    /// Every rule already defined in this package, by name.
    fn existing_rules<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let context = EvalContext::get(eval)?;
        if let Some(repos) = &context.repos {
            let heap = eval.heap();
            let names: Vec<String> = repos.borrow().iter().map(|repo| repo.name.clone()).collect();
            let entries = names.iter().map(|name| (name.as_str(), heap.alloc(AllocDict([("name", heap.alloc(name.as_str()))]))));
            return Ok(heap.alloc(AllocDict(entries.collect::<Vec<_>>())));
        }
        let builder = context.package_builder("existing_rules()")?;
        let heap = eval.heap();
        let rules: Vec<(&str, Value<'v>)> = builder
            .targets()
            .filter_map(Target::as_rule)
            .map(|rule| (rule.name.as_str(), rule_dict(&builder, rule, eval)))
            .collect();
        Ok(heap.alloc(AllocDict(rules)))
    }

    /// The subpackages beneath this package that match `include`.
    fn subpackages<'v>(
        #[starlark(require = named)] include: Value<'v>,
        #[starlark(require = named)] exclude: Option<Value<'v>>,
        #[starlark(require = named, default = false)] allow_empty: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (include, exclude, allow_empty);
        Ok(eval.heap().alloc(AllocList(Vec::<String>::new())))
    }
}

fn labels(value: Value<'_>, parser: &LabelParser<'_>) -> anyhow::Result<Vec<bazel_label::Label>> {
    let Some(items) = convert::items(value) else {
        anyhow::bail!("expected a list of labels, got {}", value.get_type());
    };
    items.into_iter().map(|item| parser.label(item)).collect()
}

fn rule_dict<'v>(builder: &crate::package_builder::PackageBuilder, rule: &bazel_packages::Rule, eval: &Evaluator<'v, '_, '_>) -> Value<'v> {
    let heap = eval.heap();
    let mut entries: Vec<(&str, Value<'v>)> = vec![
        ("name", heap.alloc(rule.name.as_str())),
        ("kind", heap.alloc(builder.rule_class(rule).kind.as_str())),
    ];
    for (attr, value) in &rule.attrs {
        entries.push((attr.as_str(), convert::to_starlark(value, heap)));
    }
    heap.alloc(AllocDict(entries))
}
