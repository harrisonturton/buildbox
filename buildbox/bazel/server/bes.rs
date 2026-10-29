//! Streaming a command's build events to the Build Event Service its
//! `--bes_backend` names, with the identity and keywords Bazel gives them.

use crate::command::{CommandContext, exit};
use bazel_bes::{BesUpload, BuildEventService, Invocation, UploadOptions};
use bazel_bep::TransportError;
use bazel_options::ParsedOptions;
use std::time::SystemTime;

/// A command's upload to a Build Event Service, and where people can see
/// it.
pub struct BesStream {
    pub upload: BesUpload,
    /// The URL `--bes_results_url` makes of the invocation, if it's given.
    pub results_url: Option<String>,
}

/// Why a command's events can't go to its Build Event Service, and the
/// exit code it ends with.
pub struct BesSetupError {
    pub message: String,
    pub exit_code: i32,
}

/// Start streaming to the Build Event Service the options name, if they
/// name one.
pub fn stream(context: &CommandContext<'_>, command: &str, options: &ParsedOptions) -> Result<Option<BesStream>, BesSetupError> {
    let Some(backend) = options.get("bes_backend").filter(|backend| !backend.is_empty()) else {
        return Ok(None);
    };
    let upload_options = upload_options(options)?;
    let mut headers = headers(options)?;
    headers.extend(helper_headers(context, options, backend)?);
    let certificates = options.get("tls_certificate").filter(|path| !path.is_empty()).map(|path| context.working_directory.join(path));
    let service = BuildEventService::new(backend, certificates.as_deref(), &headers).map_err(|error| BesSetupError {
        message: error.to_string(),
        exit_code: exit::LOCAL_ENVIRONMENTAL_ERROR,
    })?;
    let invocation = Invocation {
        build_id: options.get("build_request_id").filter(|id| !id.is_empty()).map(str::to_string).unwrap_or_else(crate::invocation::new_invocation_id),
        invocation_id: context.invocation_id.clone(),
        attempt_number: 1,
        command_start: SystemTime::now(),
        keywords: keywords(command, options),
        project_id: options.get("bes_instance_name").unwrap_or_default().to_string(),
        check_preceding_lifecycle_events: options.flag("bes_check_preceding_lifecycle_events", false),
    };
    let results_url = options.get("bes_results_url").filter(|url| !url.is_empty()).map(|url| format!("{url}{}", context.invocation_id));
    Ok(Some(BesStream {
        upload: service.upload(invocation, upload_options),
        results_url,
    }))
}

/// How the upload goes: `--bes_lifecycle_events`, `--bes_upload_mode` and
/// `--bes_timeout`.
fn upload_options(options: &ParsedOptions) -> Result<UploadOptions, BesSetupError> {
    let timeout_text = options.get("bes_timeout").unwrap_or("0s");
    let timeout = bazel_options::convert_duration(timeout_text).map_err(|message| BesSetupError {
        message: format!("While parsing option --bes_timeout={timeout_text}: {message}"),
        exit_code: exit::COMMAND_LINE_ERROR,
    })?;
    let mode = options.get("bes_upload_mode").unwrap_or("wait_for_upload_complete");
    let wait = match mode.to_ascii_lowercase().as_str() {
        "wait_for_upload_complete" => true,
        "nowait_for_upload_complete" | "fully_async" => false,
        _ => {
            return Err(BesSetupError {
                message: format!(
                    "While parsing option --bes_upload_mode={mode}: Not a valid Mode for uploading to the Build Event Service: '{mode}' (should be wait_for_upload_complete, nowait_for_upload_complete or fully_async)"
                ),
                exit_code: exit::COMMAND_LINE_ERROR,
            });
        }
    };
    Ok(UploadOptions {
        lifecycle_events: options.flag("bes_lifecycle_events", true),
        wait,
        timeout: (!timeout.is_zero()).then_some(timeout),
        ..UploadOptions::default()
    })
}

/// The headers `--bes_header` gives, each `NAME=VALUE`; the values of
/// headers of the same name are joined with commas.
fn headers(options: &ParsedOptions) -> Result<Vec<(String, String)>, BesSetupError> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for header in options.all("bes_header") {
        let Some((name, value)) = header.split_once('=') else {
            return Err(BesSetupError {
                message: format!("While parsing option --bes_header={header}: Variable definitions must be in the form of a 'name=value' assignment"),
                exit_code: exit::COMMAND_LINE_ERROR,
            });
        };
        match headers.iter_mut().find(|(given, _)| given == name) {
            Some((_, values)) => {
                values.push(',');
                values.push_str(value);
            }
            None => headers.push((name.to_string(), value.to_string())),
        }
    }
    Ok(headers)
}

/// The headers the credential helper for the service gives, if any.
fn helper_headers(context: &CommandContext<'_>, options: &ParsedOptions, backend: &str) -> Result<Vec<(String, String)>, BesSetupError> {
    let setup_error = |message: String| BesSetupError {
        message,
        exit_code: exit::LOCAL_ENVIRONMENTAL_ERROR,
    };
    let helpers = crate::command::credential_helpers(context, options).map_err(setup_error)?;
    let headers = helpers.headers(backend).map_err(setup_error)?;
    Ok(headers.unwrap_or_default())
}

/// The keywords the stream carries, as Bazel gives them: the protocol, the
/// command, `--bes_system_keywords`, then each of `--bes_keywords`.
fn keywords(command: &str, options: &ParsedOptions) -> Vec<String> {
    let mut keywords = vec!["protocol_name=BEP".to_string(), format!("command_name={command}")];
    keywords.extend(options.all("bes_system_keywords").into_iter().map(str::to_string));
    keywords.extend(options.all("bes_keywords").into_iter().map(|keyword| format!("user_keyword={keyword}")));
    let mut seen = std::collections::HashSet::new();
    keywords.retain(|keyword| seen.insert(keyword.clone()));
    keywords
}

/// The exit code a command that ended with `exit_code` ends with when its
/// upload failed: Bazel's for the failure, unless the command already
/// failed for its environment.
pub fn exit_code_after_failed_upload(exit_code: i32, error: &TransportError) -> i32 {
    if matches!(exit_code, exit::LOCAL_ENVIRONMENTAL_ERROR | exit::INTERNAL_ERROR) {
        return exit_code;
    }
    match error {
        TransportError::Upload { transient: false, .. } => exit::PERSISTENT_BES_UPLOAD_ERROR,
        TransportError::Upload { transient: true, .. } | TransportError::Timeout | TransportError::Write(_) => exit::TRANSIENT_BES_UPLOAD_ERROR,
    }
}
