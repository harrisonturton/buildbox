//! Credential helpers, as `--credential_helper` configures them: programs
//! that give the headers to authenticate requests to a URL with, following
//! the Credential Helper Specification.

use crate::netrc::url_host;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The headers a helper gave, as a request sends them.
pub type Headers = Vec<(String, String)>;

/// The credential helpers of a command, and the answers they gave.
#[derive(Debug, Default)]
pub struct CredentialHelpers {
    /// The helper for URLs no other scope matches.
    default: Option<PathBuf>,
    /// Helpers by the host they're scoped to.
    hosts: HashMap<String, PathBuf>,
    /// Helpers by the domain whose subdomains they're scoped to, as
    /// `*.example.com` gives it.
    domains: HashMap<String, PathBuf>,
    /// The environment and working directory helpers run in.
    env: Vec<(String, String)>,
    working_dir: PathBuf,
    timeout: Duration,
    cache_duration: Duration,
    cache: Mutex<HashMap<String, (Instant, Option<Headers>)>>,
}

impl CredentialHelpers {
    /// Helpers for each `(scope, path)`, a later one for a scope replacing
    /// an earlier, run in `working_dir` with `env`.
    pub fn new(helpers: Vec<(Option<String>, PathBuf)>, env: Vec<(String, String)>, working_dir: PathBuf, timeout: Duration, cache_duration: Duration) -> CredentialHelpers {
        let mut configured = CredentialHelpers {
            env,
            working_dir,
            timeout,
            cache_duration,
            ..CredentialHelpers::default()
        };
        for (scope, path) in helpers {
            match scope {
                None => configured.default = Some(path),
                Some(scope) => match scope.strip_prefix("*.") {
                    Some(domain) => {
                        configured.domains.insert(domain.to_string(), path);
                    }
                    None => {
                        configured.hosts.insert(scope, path);
                    }
                },
            }
        }
        configured
    }

    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.hosts.is_empty() && self.domains.is_empty()
    }

    /// The headers to send to `url`, or `None` if no helper's scope matches
    /// it or its helper gave none. Blocks while the helper runs.
    pub fn headers(&self, url: &str) -> Result<Option<Headers>, String> {
        let Some(helper) = self.helper(url) else {
            return Ok(None);
        };
        if let Some(cached) = self.cached(url) {
            return Ok(cached);
        }
        let headers = self.run(helper, url)?;
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.insert(url.to_string(), (Instant::now(), headers.clone()));
        Ok(headers)
    }

    /// The helper for `url`, as Bazel chooses it: the one for its host, or
    /// for the closest domain its host is in, or the default.
    fn helper(&self, url: &str) -> Option<&PathBuf> {
        let Some(host) = url_host(url).filter(|host| !host.is_empty()) else {
            return self.default.as_ref();
        };
        if let Some(helper) = self.hosts.get(host) {
            return Some(helper);
        }
        let mut domain = host;
        loop {
            if let Some(helper) = self.domains.get(domain) {
                return Some(helper);
            }
            let Some((_, parent)) = domain.split_once('.') else {
                return self.default.as_ref();
            };
            domain = parent;
        }
    }

    fn cached(&self, url: &str) -> Option<Option<Headers>> {
        let cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (at, headers) = cache.get(url)?;
        (at.elapsed() < self.cache_duration).then(|| headers.clone())
    }

    /// Ask `helper` for `url`'s headers, as the specification says: run it
    /// with `get`, the request as JSON on its standard input, and read the
    /// response from its standard output.
    fn run(&self, helper: &PathBuf, url: &str) -> Result<Option<Headers>, String> {
        let failed = |reason: String| format!("Failed to get credentials for '{url}' from helper '{}': {reason}", helper.display());
        let mut child = Command::new(helper)
            .arg("get")
            .current_dir(&self.working_dir)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| failed(error.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let request = serde_json::json!({ "uri": url }).to_string();
            let _ = stdin.write_all(request.as_bytes());
        }
        let started = Instant::now();
        while child.try_wait().map_err(|error| failed(error.to_string()))?.is_none() {
            if started.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed("process timed out".to_string()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().map_err(|error| failed(error.to_string()))?;
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(failed(format!("process exited with code {code}. stderr: {stderr}")));
        }
        parse_response(&output.stdout).map_err(|reason| failed(format!("error parsing output: {reason}")))
    }
}

/// The headers a helper's response gives, each value of a header as a
/// header of its own.
fn parse_response(stdout: &[u8]) -> Result<Option<Headers>, String> {
    let response: serde_json::Value = serde_json::from_slice(stdout).map_err(|error| error.to_string())?;
    let Some(headers) = response.get("headers") else {
        return Ok(None);
    };
    let Some(headers) = headers.as_object() else {
        return Err("'headers' must be an object".to_string());
    };
    let mut parsed = Vec::new();
    for (name, values) in headers {
        let Some(values) = values.as_array() else {
            return Err(format!("the values of header '{name}' must be a list"));
        };
        for value in values {
            let Some(value) = value.as_str() else {
                return Err(format!("the values of header '{name}' must be strings"));
            };
            parsed.push((name.clone(), value.to_string()));
        }
    }
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helpers(scopes: &[(Option<&str>, &str)]) -> CredentialHelpers {
        let helpers = scopes.iter().map(|(scope, path)| (scope.map(str::to_string), PathBuf::from(path))).collect();
        CredentialHelpers::new(helpers, Vec::new(), PathBuf::from("/"), Duration::from_secs(10), Duration::from_secs(60))
    }

    #[test]
    fn prefers_the_host_then_the_closest_domain_then_the_default() {
        let helpers = helpers(&[(None, "default"), (Some("*.example.com"), "domain"), (Some("*.a.example.com"), "subdomain"), (Some("x.example.com"), "host")]);
        let chosen = |url: &str| helpers.helper(url).map(|path| path.display().to_string());
        assert_eq!(chosen("https://x.example.com/f").as_deref(), Some("host"));
        assert_eq!(chosen("https://b.a.example.com/f").as_deref(), Some("subdomain"));
        assert_eq!(chosen("https://y.example.com/f").as_deref(), Some("domain"));
        assert_eq!(chosen("https://example.org/f").as_deref(), Some("default"));
    }

    #[test]
    fn has_no_helper_without_a_matching_scope() {
        let helpers = helpers(&[(Some("example.com"), "host")]);
        assert!(helpers.helper("https://other.com/f").is_none());
        assert_eq!(helpers.headers("https://other.com/f"), Ok(None));
    }

    #[test]
    fn reads_every_value_of_every_header() {
        let response = br#"{"headers": {"Authorization": ["Bearer a"], "X-Two": ["1", "2"]}}"#;
        let headers = parse_response(response).unwrap().unwrap();
        assert_eq!(headers, [("Authorization".to_string(), "Bearer a".to_string()), ("X-Two".to_string(), "1".to_string()), ("X-Two".to_string(), "2".to_string())]);
        assert_eq!(parse_response(b"{}").unwrap(), None);
        assert!(parse_response(br#"{"headers": {"A": "b"}}"#).is_err());
    }

    #[test]
    fn runs_helpers_as_the_specification_says() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("helper");
        std::fs::write(&script, "#!/bin/sh\n[ \"$1\" = get ] || exit 3\ncat > request.json\necho '{\"headers\": {\"Authorization\": [\"Bearer token\"]}}'\n").unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let failing = dir.path().join("failing");
        std::fs::write(&failing, "#!/bin/sh\necho nope >&2\nexit 2\n").unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&failing).status().unwrap();
        let helpers = CredentialHelpers::new(
            vec![(Some("ok.example".to_string()), script), (Some("bad.example".to_string()), failing.clone())],
            Vec::new(),
            dir.path().to_path_buf(),
            Duration::from_secs(10),
            Duration::from_secs(60),
        );
        let headers = helpers.headers("https://ok.example/f").unwrap().unwrap();
        assert_eq!(headers, [("Authorization".to_string(), "Bearer token".to_string())]);
        let request = std::fs::read_to_string(dir.path().join("request.json")).unwrap();
        assert_eq!(request, r#"{"uri":"https://ok.example/f"}"#);
        let error = helpers.headers("https://bad.example/f").unwrap_err();
        assert_eq!(error, format!("Failed to get credentials for 'https://bad.example/f' from helper '{}': process exited with code 2. stderr: nope\n", failing.display()));
    }
}
