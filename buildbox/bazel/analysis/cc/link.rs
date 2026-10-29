//! Link command lines: the arguments of a link or archive action, as the
//! toolchain's features give them, and how they go in a params file.

use super::features::{FeatureConfiguration, Variable, Variables};
use crate::args::ArgsState;

/// The variable naming the params file in a toolchain's flags.
const LINKER_PARAM_FILE: &str = "linker_param_file";

/// The text in the flag naming the params file that stands for its path.
const PLACEHOLDER: &str = "LINKER_PARAM_FILE_PLACEHOLDER";

/// The arguments of the `action` link, after the tool. With a
/// `parameter_file_type`, such as `"GCC_QUOTED"`, they go in a params file
/// of that type, named by the toolchain's flag for it, or `@<file>`.
pub fn args(configuration: &FeatureConfiguration, action: &str, variables: &Variables, parameter_file_type: Option<&str>) -> anyhow::Result<ArgsState> {
    let mut arguments = configuration.command_line(action, variables)?;
    let param_file_flag = arguments.iter().find(|argument| argument.contains(PLACEHOLDER)).cloned();
    if let Some(Variable::String(param_file)) = variables.values.get(LINKER_PARAM_FILE) {
        arguments.retain(|argument| !argument.contains(param_file.as_str()));
    }
    let Some(parameter_file_type) = parameter_file_type else {
        return Ok(ArgsState {
            arguments,
            ..ArgsState::default()
        });
    };
    let param_file = match param_file_flag {
        Some(flag) => flag.replace(PLACEHOLDER, "%s"),
        None => "@%s".to_string(),
    };
    Ok(ArgsState {
        arguments,
        param_file: Some(param_file),
        param_file_format: param_file_format(parameter_file_type)?.to_string(),
        ..ArgsState::default()
    })
}

/// The `Args` params file format of a Bazel `ParameterFileType`.
fn param_file_format(parameter_file_type: &str) -> anyhow::Result<&'static str> {
    match parameter_file_type {
        "GCC_QUOTED" => Ok("gcc_quoted"),
        "UNQUOTED" => Ok("multiline"),
        "SHELL_QUOTED" => Ok("shell"),
        "WINDOWS" => anyhow::bail!("Windows params files aren't supported by bb yet"),
        other => anyhow::bail!("unknown parameter file type '{other}'"),
    }
}
