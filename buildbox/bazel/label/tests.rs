use super::*;

fn mapping(apparent: &str) -> Option<RepoName> {
    match apparent {
        "rules_rust" => Some(RepoName::new("rules_rust+")),
        "bazel_tools" => Some(RepoName::new("bazel_tools")),
        _ => None,
    }
}

fn context(package: &str) -> LabelContext<'static> {
    LabelContext {
        package: PackageId::main(package).unwrap(),
        mapping: &mapping,
    }
}

fn parse(text: &str) -> String {
    Label::parse(text, &context("foo/bar")).unwrap().to_string()
}

#[test]
fn absolute_labels() {
    assert_eq!(parse("//a/b:c"), "//a/b:c");
    assert_eq!(parse("//a/b"), "//a/b:b");
    assert_eq!(parse("//:c"), "//:c");
    assert_eq!(parse("@//a:c"), "//a:c");
    assert_eq!(parse("//a:b/c.txt"), "//a:b/c.txt");
}

#[test]
fn relative_labels() {
    assert_eq!(parse(":c"), "//foo/bar:c");
    assert_eq!(parse("c"), "//foo/bar:c");
    assert_eq!(parse("sub/file.txt"), "//foo/bar:sub/file.txt");
}

#[test]
fn repository_labels() {
    assert_eq!(parse("@rules_rust//rust:defs.bzl"), "@@rules_rust+//rust:defs.bzl");
    assert_eq!(parse("@@rules_rust+//rust:defs.bzl"), "@@rules_rust+//rust:defs.bzl");
    assert_eq!(parse("@bazel_tools"), "@@bazel_tools//:bazel_tools");
    let in_repo = LabelContext {
        package: PackageId::new(RepoName::new("rules_rust+"), PathId::parse("rust").unwrap()),
        mapping: &mapping,
    };
    assert_eq!(Label::parse("//x:y", &in_repo).unwrap().to_string(), "@@rules_rust+//x:y");
    assert_eq!(Label::parse(":y", &in_repo).unwrap().to_string(), "@@rules_rust+//rust:y");
}

#[test]
fn canonical_form() {
    let label = Label::parse(":c", &context("a")).unwrap();
    assert_eq!(label.canonical_form(), "@@//a:c");
}

#[test]
fn invalid_labels() {
    for text in ["//a/:b", "//a//b:c", "//a:", "@unknown//a:b", "//../a:b", "a:b", "//a:../b", "@rules_rust:x"] {
        assert!(Label::parse(text, &context("x")).is_err(), "{text} should be invalid");
    }
}

fn pattern(text: &str) -> String {
    TargetPattern::parse(text, &context("foo")).unwrap().to_string()
}

#[test]
fn target_patterns() {
    assert_eq!(pattern("//a:b"), "//a:b");
    assert_eq!(pattern("//a"), "//a:a");
    assert_eq!(pattern("//a:all"), "//a:all");
    assert_eq!(pattern("//a:*"), "//a:*");
    assert_eq!(pattern("//a:all-targets"), "//a:*");
    assert_eq!(pattern("//a/..."), "//a/...");
    assert_eq!(pattern("//a/...:all"), "//a/...");
    assert_eq!(pattern("//a/...:*"), "//a/...:*");
    assert_eq!(pattern("//..."), "//...");
    assert_eq!(pattern("@rules_rust//..."), "@@rules_rust+//...");
}

#[test]
fn relative_target_patterns() {
    assert_eq!(pattern(":b"), "//foo:b");
    assert_eq!(pattern("bar:b"), "//foo/bar:b");
    assert_eq!(pattern("..."), "//foo/...");
    assert_eq!(pattern("bar/..."), "//foo/bar/...");
    assert_eq!(pattern("bar"), "//foo/bar:bar");
}
