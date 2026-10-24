use bazel_label::{Label, PackageId};
use bazel_packages::{AttrValue, Package, SelectorPart, Target};
use bazel_starlark::{EvalContext, FileKind, Globber, Loads, PackageBuilder, eval_build, eval_bzl, parse};
use intern::Symbol;
use std::collections::HashMap;

struct FakeGlobber(Vec<&'static str>);

impl Globber for FakeGlobber {
    fn glob(&self, include: &[String], _exclude: &[String], _dirs: bool, _allow_empty: bool) -> anyhow::Result<Vec<String>> {
        let suffixes: Vec<&str> = include.iter().map(|p| p.trim_start_matches('*')).collect();
        Ok(self.0.iter().filter(|f| suffixes.iter().any(|s| f.ends_with(s))).map(|f| f.to_string()).collect())
    }
}

fn label(text: &str) -> Label {
    Label::parse_canonical(text).unwrap()
}

const DEFS: &str = r#"
MyInfo = provider(fields = ["value"])

def _impl(ctx):
    return [MyInfo(value = 1)]

my_rule = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "out": attr.output(),
        "flag": attr.bool(default = True),
        "_tool": attr.label(default = "//tools:compiler", cfg = "exec"),
    },
)

def my_macro(name, **kwargs):
    my_rule(name = name + "_inner", **kwargs)
    native.filegroup(name = name, srcs = [":" + name + "_inner"])

LABEL = Label("//tools:x")
"#;

const BUILD: &str = r#"
load(":defs.bzl", "LABEL", "my_macro", "my_rule")

package(default_visibility = ["//visibility:public"])

my_rule(
    name = "a",
    srcs = glob(["*.c"]) + select({
        ":opt": ["opt.c"],
        "//conditions:default": [],
    }),
    out = "a.out",
)

my_macro(name = "b", flag = False)

genrule(
    name = "gen",
    outs = ["gen.txt"],
    cmd = "echo {} > $@".format(str(LABEL)),
)

exports_files(["exported.txt"])
"#;

fn load_package() -> Package {
    let defs_label = label("//pkg:defs.bzl");
    let defs = parse(defs_label, DEFS.to_string()).unwrap();
    let (defs_module, _) = eval_bzl(defs, &Loads { modules: HashMap::new() }, EvalContext::new(FileKind::Bzl, defs_label)).unwrap();

    let build_label = label("//pkg:BUILD");
    let build = parse(build_label, BUILD.to_string()).unwrap();
    let loads = Loads {
        modules: HashMap::from([(":defs.bzl".to_string(), &defs_module)]),
    };
    let mut context = EvalContext::new(FileKind::Build, build_label);
    context.globber = Some(Box::new(FakeGlobber(vec!["x.c", "y.c", "z.h"])));
    let builder = PackageBuilder::new(PackageId::main("pkg").unwrap(), Symbol::intern("BUILD"));
    let (package, _) = eval_build(build, &loads, context, builder);
    package
}

#[test]
fn build_files_define_targets() {
    let package = load_package();
    assert_eq!(package.errors, Vec::<String>::new());
    let names: Vec<&str> = package.targets.iter().map(|t| t.name().as_str()).collect();
    assert_eq!(names, ["BUILD", "a", "a.out", "b", "b_inner", "exported.txt", "gen", "gen.txt", "opt.c", "x.c", "y.c"]);
    assert_eq!(package.args.default_visibility, [label("//visibility:public")]);
}

#[test]
fn rules_record_their_attributes() {
    let package = load_package();
    let Some(Target::Rule(a)) = package.target(Symbol::intern("a")) else {
        panic!("a is a rule");
    };
    assert_eq!(package.rule_class(a).kind.as_str(), "my_rule");
    assert_eq!(package.rule_class(a).definition, Some((label("//pkg:defs.bzl"), Symbol::intern("my_rule"))));
    let Some(AttrValue::Configurable(parts)) = a.attr(Symbol::intern("srcs")) else {
        panic!("srcs is configurable");
    };
    assert_eq!(parts[0], SelectorPart::Value(AttrValue::LabelList(vec![label("//pkg:x.c"), label("//pkg:y.c")])));
    assert!(matches!(&parts[1], SelectorPart::Select(select) if select.branches[0].0 == label("//pkg:opt")));
    assert_eq!((a.location.line, a.location.column), (6, 8));
    assert_eq!(a.outputs, [Symbol::intern("a.out")]);
}

#[test]
fn macros_record_their_generator() {
    let package = load_package();
    let Some(Target::Rule(inner)) = package.target(Symbol::intern("b_inner")) else {
        panic!("b_inner is a rule");
    };
    let generator = inner.generator.as_ref().expect("created by a macro");
    assert_eq!(generator.function.as_str(), "my_macro");
    assert_eq!(inner.attr(Symbol::intern("flag")), Some(&AttrValue::Bool(false)));
}

#[test]
fn labels_resolve_relative_to_their_file() {
    let package = load_package();
    let Some(Target::Rule(gen)) = package.target(Symbol::intern("gen")) else {
        panic!("gen is a rule");
    };
    assert_eq!(gen.attr(Symbol::intern("cmd")), Some(&AttrValue::String("echo @@//tools:x > $@".into())));
}

#[test]
fn errors_keep_earlier_targets() {
    let build_label = label("//pkg:BUILD");
    let build = parse(build_label, "filegroup(name = 'ok')\nfilegroup(name = 'bad', nope = 1)\n".to_string()).unwrap();
    let context = EvalContext::new(FileKind::Build, build_label);
    let builder = PackageBuilder::new(PackageId::main("pkg").unwrap(), Symbol::intern("BUILD"));
    let (package, _) = eval_build(build, &Loads { modules: HashMap::new() }, context, builder);
    assert!(package.target(Symbol::intern("ok")).is_some());
    assert_eq!(package.errors.len(), 1);
    assert!(package.errors[0].contains("no such attribute 'nope'"), "{}", package.errors[0]);
}
