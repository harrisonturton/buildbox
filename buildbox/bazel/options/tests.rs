use super::*;

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn args(text: &str) -> Vec<Arg> {
    given_args(&words(text))
}

fn texts(args: &[Arg]) -> Vec<String> {
    args.iter().map(|arg| arg.text.clone()).collect()
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
    let parsed = parse(&args("--output label_kind --noimplicit_deps -k deps(//foo) --copt=-O2 --copt -g"), &spec(), &rc, "cquery").unwrap();
    assert_eq!(parsed.get("output"), Some("label_kind"));
    assert!(!parsed.flag("implicit_deps", true));
    assert!(parsed.flag("keep_going", false));
    assert_eq!(parsed.all("copt"), ["-O2", "-g"]);
    assert_eq!(parsed.residue, ["deps(//foo)"]);
}

#[test]
fn options_bazel_doesnt_take_are_errors() {
    let rc = RcFile::default();
    for given in ["--nonsense", "--output=label"] {
        let error = parse(&args(&format!("{given} //foo")), &spec(), &rc, "build").unwrap_err();
        assert_eq!(error.to_string(), format!("{given} :: Unrecognized option: {given}"));
    }
}

#[test]
fn options_bb_lacks_are_errors_unless_given_their_defaults() {
    let rc = RcFile::default();
    for (given, spelled) in [("--remote_cache=grpc://x", "--remote_cache=grpc://x"), ("--remote_cache grpc://x", "--remote_cache grpc://x"), ("--announce_rc", "--announce_rc")] {
        let error = parse(&args(given), &spec(), &rc, "build").unwrap_err();
        let name = given.trim_start_matches("--").split(['=', ' ']).next().unwrap();
        assert_eq!(error.to_string(), format!("{spelled} :: bb doesn't support --{name} yet"));
    }
    let parsed = parse(&args("--remote_cache= --noannounce_rc --show_timestamps=false --bes_backend= //foo"), &spec(), &rc, "build");
    assert_eq!(parsed.map(|parsed| parsed.residue), Ok(words("//foo")));
}

#[test]
fn only_the_last_use_of_an_unsupported_option_counts() {
    let rc = rc::parse_for_test("build --disk_cache=/cache --remote_header=a=b\n");
    let reset = command_args(&rc, "build", &words("--disk_cache= --remote_header="));
    let error = parse(&reset, &spec(), &rc, "build").unwrap_err();
    assert_eq!(error.to_string(), "--remote_header=a=b :: bb doesn't support --remote_header yet");
    let rc = rc::parse_for_test("build --disk_cache=/cache\n");
    let reset = command_args(&rc, "build", &words("--disk_cache="));
    assert!(parse(&reset, &spec(), &rc, "build").is_ok());
}

#[test]
fn common_options_apply_only_to_commands_that_take_them() {
    let rc = rc::parse_for_test("common --output=label --implicit_deps\nbuild:x --output=label\ncommon:y --output=label\n");
    let build = command_args(&rc, "build", &words("//foo"));
    assert_eq!(parse(&build, &spec(), &rc, "build").map(|parsed| parsed.residue), Ok(words("//foo")));
    let query = parse(&command_args(&rc, "query", &[]), &spec(), &rc, "query").unwrap();
    assert_eq!(query.get("output"), Some("label"));
    let error = parse(&args("--config=x"), &spec(), &rc, "build").unwrap_err();
    assert_eq!(error, OptionsError::Unrecognized("--output=label".into()));
    assert!(parse(&args("--config=y"), &spec(), &rc, "build").is_ok());

    let rc = rc::parse_for_test("common --nonsense\n");
    let error = parse(&command_args(&rc, "build", &[]), &spec(), &rc, "build").unwrap_err();
    assert_eq!(error, OptionsError::Unrecognized("--nonsense".into()));
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
        "# comment\ncommon --keep_going\nbuild --copt=-O1\nbuild:opt --copt=-O3\ntest --ui_actions_shown=3\nquery --output=label_kind\n",
    );
    let build = command_args(&rc, "test", &words("--config=opt //..."));
    assert_eq!(texts(&build), words("--keep_going --copt=-O1 --ui_actions_shown=3 --config=opt //..."));
    let parsed = parse(&build, &spec(), &rc, "test").unwrap();
    assert_eq!(parsed.all("copt"), ["-O1", "-O3"]);

    let query = command_args(&rc, "query", &[]);
    assert_eq!(texts(&query), words("--keep_going --output=label_kind"));
}

#[cfg(target_os = "linux")]
#[test]
fn platform_specific_configs_apply_when_enabled() {
    let rc = rc::parse_for_test("common --enable_platform_specific_config\nbuild:linux --copt=-DLINUX\n");
    let build = command_args(&rc, "build", &words("//..."));
    assert_eq!(texts(&build), words("--enable_platform_specific_config --config=linux //..."));
    let off = command_args(&rc, "build", &words("--noenable_platform_specific_config //..."));
    assert_eq!(texts(&off), words("--enable_platform_specific_config --noenable_platform_specific_config //..."));
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

#[test]
fn bazels_options_table_parses() {
    let jobs = bazel_option("jobs").unwrap();
    assert_eq!((jobs.kind, jobs.default), (BazelKind::Value, Some("auto")));
    assert!(jobs.takes("build") && !jobs.takes("query"));
    assert!(bazel_option("remote_cache").unwrap().is_default(""));
}

#[test]
fn tristate_options_take_auto() {
    let rc = RcFile::default();
    let spec = OptionSpec::new(&[("cache_test_results", OptionKind::TriState)]);
    for (given, value) in [("--cache_test_results", "true"), ("--nocache_test_results", "false"), ("--cache_test_results=auto", "auto"), ("--cache_test_results=no", "false")] {
        let parsed = parse(&args(given), &spec, &rc, "test").unwrap();
        assert_eq!(parsed.get("cache_test_results"), Some(value), "{given}");
    }
    let error = parse(&args("--cache_test_results=maybe"), &spec, &rc, "test").unwrap_err();
    assert_eq!(error.to_string(), "While parsing option --cache_test_results=maybe: 'maybe' is not a boolean or 'auto'");
}
