use super::*;

fn args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn spec() -> OptionSpec {
    OptionSpec::new(&[
        ("keep_going", OptionKind::Bool),
        ("implicit_deps", OptionKind::Bool),
        ("output", OptionKind::Value),
        ("copt", OptionKind::Multi),
        ("ui_actions_shown", OptionKind::Int),
        ("jobs", OptionKind::Jobs),
    ])
}

#[test]
fn jobs_are_recorded_as_a_count() {
    let rc = RcFile::default();
    let parsed = parse(&args("--jobs=4 //foo"), &spec(), &rc, "build").unwrap();
    assert_eq!(parsed.get("jobs"), Some("4"));
    let parsed = parse(&args("-j 6000 //foo"), &spec(), &rc, "build").unwrap();
    assert_eq!(parsed.get("jobs"), Some("5000"));
    assert_eq!(parsed.residue, ["//foo"]);
}

#[test]
fn jobs_errors_name_the_option_as_given() {
    let rc = RcFile::default();
    for (given, spelled) in [("--jobs=0", "--jobs=0"), ("--jobs 0", "--jobs 0"), ("-j 0", "-j 0")] {
        let error = parse(&args(given), &spec(), &rc, "build").unwrap_err();
        assert_eq!(error.to_string(), format!("While parsing option {spelled}: Value '(0)' must be at least 1."));
    }
}

#[test]
fn j_is_a_target_where_jobs_isnt_an_option() {
    let rc = RcFile::default();
    let spec = OptionSpec::new(&[("output", OptionKind::Value)]);
    let parsed = parse(&args("-j 4"), &spec, &rc, "query").unwrap();
    assert_eq!(parsed.residue, ["-j", "4"]);
}

#[test]
fn integer_options_must_be_integers() {
    let rc = RcFile::default();
    let parsed = parse(&args("--ui_actions_shown=12"), &spec(), &rc, "build").unwrap();
    assert_eq!(parsed.int("ui_actions_shown", 8), 12);
    let error = parse(&args("--ui_actions_shown=many"), &spec(), &rc, "build").unwrap_err();
    assert_eq!(error.to_string(), "While parsing option --ui_actions_shown=many: 'many' is not an int");
}

#[test]
fn parses_options_and_residue() {
    let rc = RcFile::default();
    let parsed = parse(&args("--output label_kind --noimplicit_deps -k deps(//foo) --copt=-O2 --copt -g"), &spec(), &rc, "query").unwrap();
    assert_eq!(parsed.get("output"), Some("label_kind"));
    assert!(!parsed.flag("implicit_deps", true));
    assert!(parsed.flag("keep_going", false));
    assert_eq!(parsed.all("copt"), ["-O2", "-g"]);
    assert_eq!(parsed.residue, ["deps(//foo)"]);
}

#[test]
fn unknown_options_are_skipped() {
    let rc = RcFile::default();
    let parsed = parse(&args("--bes_backend=grpc://x --announce_rc //foo:bar"), &spec(), &rc, "build").unwrap();
    assert_eq!(parsed.unknown, ["--bes_backend=grpc://x", "--announce_rc"]);
    assert_eq!(parsed.residue, ["//foo:bar"]);
}

#[test]
fn double_dash_ends_options() {
    let rc = RcFile::default();
    let parsed = parse(&args("--keep_going -- -//foo:bar"), &spec(), &rc, "build").unwrap();
    assert_eq!(parsed.residue, ["-//foo:bar"]);
}

#[test]
fn rc_files_add_options_by_command_and_config() {
    let rc = rc::parse_for_test(
        "# comment\ncommon --keep_going\nbuild --copt=-O1\nbuild:opt --copt=-O3\ntest --output=x\nquery --output=label_kind\n",
    );
    let build = command_args(&rc, "test", &args("--config=opt //..."));
    assert_eq!(build, args("--keep_going --copt=-O1 --output=x --config=opt //..."));
    let parsed = parse(&build, &spec(), &rc, "test").unwrap();
    assert_eq!(parsed.all("copt"), ["-O1", "-O3"]);

    let query = command_args(&rc, "query", &[]);
    assert_eq!(query, args("--keep_going --output=label_kind"));
}

#[cfg(target_os = "linux")]
#[test]
fn platform_specific_configs_apply_when_enabled() {
    let rc = rc::parse_for_test("common --enable_platform_specific_config\nbuild:linux --copt=-DLINUX\n");
    let build = command_args(&rc, "build", &args("//..."));
    assert_eq!(build, args("--enable_platform_specific_config --config=linux //..."));
    let off = command_args(&rc, "build", &args("--noenable_platform_specific_config //..."));
    assert_eq!(off, args("--enable_platform_specific_config --noenable_platform_specific_config //..."));
}

#[test]
fn undefined_configs_are_errors() {
    let rc = RcFile::default();
    let error = parse(&args("--config=nope"), &spec(), &rc, "build").unwrap_err();
    assert_eq!(error, OptionsError::UnknownConfig("nope".into()));
}

#[test]
fn rc_lines_are_split_like_a_shell() {
    let rc = rc::parse_for_test("build --define=a='b c' \\\n  --copt=\"-DX=1\"\n");
    assert_eq!(rc.command_args("build"), ["--define=a=b c", "--copt=-DX=1"]);
}
