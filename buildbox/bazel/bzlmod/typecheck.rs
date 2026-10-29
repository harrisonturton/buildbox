//! Checking the attributes a repository rule is called with against its
//! schema, with Bazel's errors, as Bazel checks an override's before it
//! runs the rule.

use crate::star_value::StarValue;
use bazel_packages::AttrType;

/// One attribute of a rule's schema.
pub struct SchemaAttr<'a> {
    pub name: &'a str,
    pub ty: AttrType,
    pub mandatory: bool,
}

/// Check `given` against `schema`: that each is an attribute the schema
/// has, of its type, and that every mandatory attribute is given. `None`
/// counts as not given. `call` names the call in errors, such as
/// `call to 'git_repository' repo rule`.
pub fn check(call: &str, schema: &[SchemaAttr<'_>], given: &[(String, StarValue)]) -> Result<(), String> {
    for (name, value) in given {
        if *value == StarValue::None {
            continue;
        }
        let Some(attr) = schema.iter().find(|attr| attr.name == name) else {
            let names: Vec<String> = schema.iter().map(|attr| attr.name.to_string()).collect();
            let suggestion = bazel_starlark::reported::did_you_mean(name, &names);
            return Err(format!("in {call}, unknown attribute '{name}' provided{suggestion}"));
        };
        let what = format!("attribute '{name}'");
        check_value(attr.ty, value, &what).map_err(|error| format!("in {call}, {error}"))?;
    }
    for attr in schema {
        let given = given.iter().any(|(name, value)| name == attr.name && *value != StarValue::None);
        if attr.mandatory && !given {
            return Err(format!("in {call}, mandatory attribute '{}' isn't being specified", attr.name));
        }
    }
    Ok(())
}

/// Check that `value` has type `ty`; `what` says where it is in errors.
fn check_value(ty: AttrType, value: &StarValue, what: &str) -> Result<(), String> {
    match ty {
        AttrType::Bool => check_bool(value, what),
        AttrType::Int | AttrType::Tristate => expect(matches!(value, StarValue::Int(_)), ty, value, what),
        AttrType::String => expect(matches!(value, StarValue::String(_)), ty, value, what),
        AttrType::Label => expect(is_label(value), ty, value, what),
        AttrType::StringList => check_list(value, AttrType::String, ty, what),
        AttrType::LabelList => check_list(value, AttrType::Label, ty, what),
        AttrType::IntList => check_list(value, AttrType::Int, ty, what),
        AttrType::StringDict => check_dict(value, AttrType::String, AttrType::String, ty, what),
        AttrType::StringListDict => check_dict(value, AttrType::String, AttrType::StringList, ty, what),
        AttrType::LabelKeyedStringDict => check_dict(value, AttrType::Label, AttrType::String, ty, what),
        AttrType::StringKeyedLabelDict => check_dict(value, AttrType::String, AttrType::Label, ty, what),
        AttrType::Output | AttrType::OutputList | AttrType::License => Ok(()),
    }
}

fn is_label(value: &StarValue) -> bool {
    matches!(value, StarValue::String(_) | StarValue::Label(_))
}

/// Booleans may also be given as `0` or `1`.
fn check_bool(value: &StarValue, what: &str) -> Result<(), String> {
    match value {
        StarValue::Bool(_) | StarValue::Int(0 | 1) => Ok(()),
        _ => Err(mismatch("one of [False, True, 0, 1]", value, what)),
    }
}

fn check_list(value: &StarValue, element: AttrType, ty: AttrType, what: &str) -> Result<(), String> {
    let StarValue::List(items) = value else {
        return Err(mismatch(&format!("value of type '{}'", type_name(ty)), value, what));
    };
    for (index, item) in items.iter().enumerate() {
        check_value(element, item, &format!("element {index} of {what}"))?;
    }
    Ok(())
}

fn check_dict(value: &StarValue, key: AttrType, entry: AttrType, ty: AttrType, what: &str) -> Result<(), String> {
    let StarValue::Dict(entries) = value else {
        return Err(mismatch(&format!("value of type '{}'", type_name(ty)), value, what));
    };
    for (k, v) in entries {
        check_value(key, k, "dict key element")?;
        check_value(entry, v, "dict value element")?;
    }
    Ok(())
}

fn expect(matches: bool, ty: AttrType, value: &StarValue, what: &str) -> Result<(), String> {
    match matches {
        true => Ok(()),
        false => Err(mismatch(&format!("value of type '{}'", type_name(ty)), value, what)),
    }
}

/// Bazel's error for a value of the wrong type.
fn mismatch(expected: &str, value: &StarValue, what: &str) -> String {
    format!("expected {expected} for {what}, but got {} ({})", repr(value), value.type_name())
}

/// The name Bazel gives an attribute type in errors.
fn type_name(ty: AttrType) -> &'static str {
    match ty {
        AttrType::Bool => "boolean",
        AttrType::Int => "int",
        AttrType::String => "string",
        AttrType::Label => "label",
        AttrType::StringList => "list(string)",
        AttrType::LabelList => "list(label)",
        AttrType::IntList => "list(int)",
        AttrType::StringDict => "dict(string, string)",
        AttrType::StringListDict => "dict(string, list(string))",
        AttrType::LabelKeyedStringDict => "dict(label, string)",
        AttrType::StringKeyedLabelDict => "dict(string, label)",
        AttrType::Tristate => "tristate",
        AttrType::Output => "output",
        AttrType::OutputList => "list(output)",
        AttrType::License => "license",
    }
}

/// A value as Starlark's `repr` writes it.
fn repr(value: &StarValue) -> String {
    match value {
        StarValue::None => "None".to_string(),
        StarValue::Bool(true) => "True".to_string(),
        StarValue::Bool(false) => "False".to_string(),
        StarValue::Int(i) => i.to_string(),
        StarValue::String(text) => format!("{text:?}"),
        StarValue::Label(label) => format!("Label({:?})", label.to_string()),
        StarValue::List(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        StarValue::Dict(entries) => {
            let entries: Vec<String> = entries.iter().map(|(k, v)| format!("{}: {}", repr(k), repr(v))).collect();
            format!("{{{}}}", entries.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL: &str = "call to 'git_repository' repo rule";

    fn schema() -> Vec<SchemaAttr<'static>> {
        vec![
            SchemaAttr { name: "remote", ty: AttrType::String, mandatory: true },
            SchemaAttr { name: "patch_strip", ty: AttrType::Int, mandatory: false },
            SchemaAttr { name: "patches", ty: AttrType::LabelList, mandatory: false },
            SchemaAttr { name: "verbose", ty: AttrType::Bool, mandatory: false },
            SchemaAttr { name: "remote_patches", ty: AttrType::StringDict, mandatory: false },
        ]
    }

    fn given(attrs: &[(&str, StarValue)]) -> Vec<(String, StarValue)> {
        attrs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect()
    }

    fn string(text: &str) -> StarValue {
        StarValue::String(text.to_string())
    }

    #[test]
    fn accepts_values_of_the_schemas_types() {
        let attrs = given(&[
            ("remote", string("https://x")),
            ("patch_strip", StarValue::Int(1)),
            ("patches", StarValue::List(vec![string("//:a.patch")])),
            ("verbose", StarValue::Int(1)),
            ("remote_patches", StarValue::Dict(vec![(string("https://p"), string("sha256-x"))])),
        ]);
        assert_eq!(check(CALL, &schema(), &attrs), Ok(()));
    }

    #[test]
    fn unknown_attributes_are_errors_with_a_suggestion() {
        let attrs = given(&[("remote", string("x")), ("remotes", string("y"))]);
        let error = check(CALL, &schema(), &attrs).unwrap_err();
        assert_eq!(error, "in call to 'git_repository' repo rule, unknown attribute 'remotes' provided (did you mean 'remote'?)");
    }

    #[test]
    fn mandatory_attributes_must_be_given_other_than_none() {
        let attrs = given(&[("remote", StarValue::None)]);
        let error = check(CALL, &schema(), &attrs).unwrap_err();
        assert_eq!(error, "in call to 'git_repository' repo rule, mandatory attribute 'remote' isn't being specified");
    }

    #[test]
    fn type_errors_say_what_was_expected_and_given() {
        let attrs = given(&[("remote", string("x")), ("patch_strip", string("1"))]);
        let error = check(CALL, &schema(), &attrs).unwrap_err();
        assert_eq!(error, "in call to 'git_repository' repo rule, expected value of type 'int' for attribute 'patch_strip', but got \"1\" (string)");

        let attrs = given(&[("remote", string("x")), ("patches", StarValue::List(vec![string("//:a"), StarValue::Int(2)]))]);
        let error = check(CALL, &schema(), &attrs).unwrap_err();
        assert_eq!(error, "in call to 'git_repository' repo rule, expected value of type 'label' for element 1 of attribute 'patches', but got 2 (int)");

        let attrs = given(&[("remote", string("x")), ("verbose", string("yes"))]);
        let error = check(CALL, &schema(), &attrs).unwrap_err();
        assert_eq!(error, "in call to 'git_repository' repo rule, expected one of [False, True, 0, 1] for attribute 'verbose', but got \"yes\" (string)");
    }
}
