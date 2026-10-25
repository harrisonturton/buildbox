use bazel_label::{Label, PackageId};
use bazel_packages::{AttrValue, Package, SelectorPart, Target};
use bazel_starlark::{BUILD_GLOBALS, BZL_GLOBALS, EvalContext, FileKind, Globber, Loads, PackageBuilder, eval_build, eval_bzl, heap_name, parse};
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
    let defs_module = eval_bzl(defs, &BZL_GLOBALS, &Loads { modules: HashMap::new() }, EvalContext::new(FileKind::Bzl, defs_label), heap_name("test", defs_label, 0)).0.unwrap();

    let build_label = label("//pkg:BUILD");
    let build = parse(build_label, BUILD.to_string()).unwrap();
    let loads = Loads {
        modules: HashMap::from([(":defs.bzl".to_string(), &defs_module)]),
    };
    let mut context = EvalContext::new(FileKind::Build, build_label);
    context.globber = Some(Box::new(FakeGlobber(vec!["x.c", "y.c", "z.h"])));
    let builder = PackageBuilder::new(PackageId::main("pkg").unwrap(), Symbol::intern("BUILD"));
    let (package, _) = eval_build(build, &BUILD_GLOBALS, &loads, context, builder);
    package
}

#[test]
fn build_files_define_targets() {
    let package = load_package();
    assert!(package.errors.is_empty(), "{:?}", package.errors);
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
    assert_eq!(generator.name.as_str(), "b");
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

fn evaluate_build(source: &str) -> Package {
    let build_label = label("//pkg:BUILD");
    let build = parse(build_label, source.to_string()).unwrap();
    let context = EvalContext::new(FileKind::Build, build_label);
    let builder = PackageBuilder::new(PackageId::main("pkg").unwrap(), Symbol::intern("BUILD"));
    eval_build(build, &BUILD_GLOBALS, &Loads { modules: HashMap::new() }, context, builder).0
}

#[test]
fn invalid_rules_are_errors_at_their_call() {
    let package = evaluate_build("filegroup(name = 'ok')\nfilegroup(name = 'bad', nope = 1)\nfilegroup(name = 'after')\n");
    assert!(package.target(Symbol::intern("ok")).is_some());
    assert!(package.target(Symbol::intern("bad")).is_some());
    assert!(package.target(Symbol::intern("after")).is_some());
    assert_eq!(package.errors.len(), 1);
    assert_eq!(package.errors[0].to_string(), "//pkg:BUILD:2:10: //pkg:bad: no such attribute 'nope' in 'filegroup' rule");
}

#[test]
fn failures_keep_earlier_targets_and_their_traceback() {
    let package = evaluate_build("filegroup(name = 'ok')\nfail('boom')\nfilegroup(name = 'after')\n");
    assert!(package.target(Symbol::intern("ok")).is_some());
    assert!(package.target(Symbol::intern("after")).is_none());
    let expected = "Traceback (most recent call last):\n\tFile \"//pkg:BUILD\", line 2, column 5, in <toplevel>\n\t\tfail('boom')\nError in fail: boom";
    assert_eq!(package.errors[0].to_string(), expected);
}

#[test]
fn every_undefined_name_is_an_error() {
    let package = evaluate_build("foo()\nfilegroup(name = 'ok', srcs = bar)\n");
    let errors: Vec<String> = package.errors.iter().map(ToString::to_string).collect();
    assert_eq!(errors, ["//pkg:BUILD:1:1: name 'foo' is not defined", "//pkg:BUILD:2:31: name 'bar' is not defined"]);
}

#[test]
fn undefined_names_are_errors_at_their_position_with_a_suggestion() {
    let package = evaluate_build("filegroup(name = 'ok')\nselct({})\n");
    assert!(package.targets.iter().all(|target| target.name().as_str() != "ok"));
    assert_eq!(package.errors[0].to_string(), "//pkg:BUILD:2:1: name 'selct' is not defined (did you mean 'select'?)");
}

#[test]
fn package_can_follow_targets() {
    let build_label = label("//pkg:BUILD");
    let source = "exports_files(['LICENSE'])\npackage(default_visibility = ['//visibility:public'])\nfilegroup(name = 'ok')\n";
    let build = parse(build_label, source.to_string()).unwrap();
    let context = EvalContext::new(FileKind::Build, build_label);
    let builder = PackageBuilder::new(PackageId::main("pkg").unwrap(), Symbol::intern("BUILD"));
    let (package, _) = eval_build(build, &BUILD_GLOBALS, &Loads { modules: HashMap::new() }, context, builder);
    assert!(package.errors.is_empty(), "{:?}", package.errors);
    assert_eq!(package.args.default_visibility, [label("//visibility:public")]);
}

#[test]
fn accepts_bazels_dialect() {
    let file = label("//pkg:dialect.bzl");
    let source = r#"
UNION = {"a": "1"} | select({"//conditions:default": {"b": "2"}})
DEFAULT = {}.get("k", default = 3)
ELEMS = "abc".elems()[1:]
"#;
    let ast = parse(file, source.to_string()).unwrap();
    let module = eval_bzl(ast, &BZL_GLOBALS, &Loads { modules: HashMap::new() }, EvalContext::new(FileKind::Bzl, file), heap_name("test", file, 0)).0.unwrap();
    assert_eq!(module.get("UNION").unwrap().value().get_type(), "select");
    assert_eq!(module.get("DEFAULT").unwrap().value().unpack_i32(), Some(3));
    assert_eq!(module.get("ELEMS").unwrap().value().to_repr(), r#"["b", "c"]"#);
}

#[test]
fn callables_print_as_bazel_prints_them() {
    let file = label("@@dep+//pkg:callables.bzl");
    let source = r#"
def f():
    pass

def _impl(ctx):
    pass

r = rule(implementation = _impl)
m = macro(implementation = _impl)

NAMES = [str(f), str(lambda: None), str(r), str(m)]
"#;
    let ast = parse(file, source.to_string()).unwrap();
    let module = eval_bzl(ast, &BZL_GLOBALS, &Loads { modules: HashMap::new() }, EvalContext::new(FileKind::Bzl, file), heap_name("test", file, 0)).0.unwrap();
    assert_eq!(
        module.get("NAMES").unwrap().value().to_repr(),
        r#"["<function f from @@dep+//pkg:callables.bzl>", "<function lambda from @@dep+//pkg:callables.bzl>", "<rule r>", "<macro m>"]"#
    );
}

