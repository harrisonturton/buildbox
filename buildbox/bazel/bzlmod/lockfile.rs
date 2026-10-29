//! `MODULE.bazel.lock`, as Bazel 9 reads and writes it: the hashes of the
//! registry files resolution read, the yanked versions it selected, and
//! the results of module extensions that aren't reproducible.
//!
//! bb computes the first two itself. It can't compute Bazel's digests of
//! module extensions, so it keeps the extension entries and facts the file
//! has, for the extensions still in use, as they're written.

use crate::resolve::DepGraph;
use bazel_fs::{Root, RootedPath};
use intern::PathId;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The lockfile's name, in the workspace.
pub const LOCKFILE: &str = "MODULE.bazel.lock";

/// The version of the lockfile's format Bazel 9.3.1 writes.
pub const LOCKFILE_VERSION: u64 = 28;

/// What a registry file's hash is recorded as when the registry didn't
/// have it.
const NOT_FOUND: &str = "not found";

/// How a command uses the lockfile: `--lockfile_mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum LockfileMode {
    /// Neither read nor write it.
    Off,
    /// Trust what it records, and add what's new.
    #[default]
    Update,
    /// Trust the files it records, but check mutable registry information,
    /// such as yanked versions, again.
    Refresh,
    /// Require it to have everything resolution needs, and never change it.
    Error,
}

impl LockfileMode {
    /// The mode `--lockfile_mode` names, as Bazel spells it.
    pub fn parse(text: &str) -> Option<LockfileMode> {
        match text.to_ascii_lowercase().as_str() {
            "off" => Some(LockfileMode::Off),
            "update" => Some(LockfileMode::Update),
            "refresh" => Some(LockfileMode::Refresh),
            "error" => Some(LockfileMode::Error),
            _ => None,
        }
    }

    /// Whether the command writes the lockfile when what it records changes.
    pub fn writes(self) -> bool {
        matches!(self, LockfileMode::Update | LockfileMode::Refresh)
    }
}

/// A graph service: the lockfile mode of the latest command.
#[derive(Debug, Default)]
pub struct CommandLockfileMode(Mutex<LockfileMode>);

impl CommandLockfileMode {
    pub fn new() -> Arc<CommandLockfileMode> {
        Arc::default()
    }

    /// Set the mode, returning whether it changed, in which case
    /// [`LockfilePolicyKey`] must be invalidated.
    pub fn set(&self, mode: LockfileMode) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = *current != mode;
        *current = mode;
        changed
    }

    fn current(&self) -> LockfileMode {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A lockfile's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Lockfile {
    /// Registry files by URL, with their SHA-256 in hex, `None` for those
    /// the registry didn't have.
    pub registry_file_hashes: BTreeMap<String, Option<String>>,
    /// Selected versions that were yanked, `name@version`, with why.
    pub selected_yanked_versions: BTreeMap<String, String>,
    /// The `moduleExtensions` entries, by extension id, as written.
    pub module_extensions: Vec<(String, String)>,
    /// The `facts` entries, by extension id, as written.
    pub facts: Vec<(String, String)>,
    /// The `factsVersions` entries, by extension id, as written.
    pub facts_versions: Vec<(String, String)>,
}

encoded_value!(Lockfile);

/// Why a lockfile couldn't be used.
#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
    #[error("Failed to read and parse the MODULE.bazel.lock file with error: {message}.{suffix}", suffix = parse_suffix(*.conflict))]
    Parse { message: String, conflict: bool },
    #[error("The version of MODULE.bazel.lock is not supported by this version of bb. Please run `bb mod deps --lockfile_mode=update` to update your lockfile.")]
    Version,
}

/// Whether an error is the lockfile's own, which Bazel reports by itself
/// rather than as the main repository mapping's failure.
pub fn is_lockfile_error(message: &str) -> bool {
    const STARTS: [&str; 4] = [
        "Missing checksum for registry file",
        "Failed to read and parse the MODULE.bazel.lock",
        "The version of MODULE.bazel.lock",
        "The module extension '",
    ];
    STARTS.iter().any(|start| message.starts_with(start))
}

/// What Bazel suggests after a lockfile that doesn't parse, which may be
/// one with merge conflicts.
fn parse_suffix(conflict: bool) -> &'static str {
    match conflict {
        true => " This looks like a merge conflict. See https://bazel.build/external/lockfile#merge-conflicts for advice.",
        false => " Try deleting it and rerun the build.",
    }
}

impl Lockfile {
    /// The lockfile `text` holds. One of another version is empty, as Bazel
    /// ignores it, unless `mode` is `error`.
    pub fn parse(text: &str, mode: LockfileMode) -> Result<Lockfile, LockfileError> {
        let parse_error = |message: String| LockfileError::Parse {
            message,
            conflict: text.lines().any(|line| line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>")),
        };
        let json: serde_json::Value = serde_json::from_str(text).map_err(|error| parse_error(error.to_string()))?;
        let version = json.get("lockFileVersion").and_then(serde_json::Value::as_u64);
        if version != Some(LOCKFILE_VERSION) {
            return match mode {
                LockfileMode::Error => Err(LockfileError::Version),
                _ => Ok(Lockfile::default()),
            };
        }
        let strings = |name: &str| -> BTreeMap<String, String> {
            let entries = json.get(name).and_then(serde_json::Value::as_object);
            let entries = entries.into_iter().flatten();
            entries.filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string()))).collect()
        };
        let registry_file_hashes = strings("registryFileHashes")
            .into_iter()
            .map(|(url, hash)| (url, (hash != NOT_FOUND).then_some(hash)))
            .collect();
        let members = top_level_members(text).map_err(parse_error)?;
        let raw = |name: &str| -> Result<Vec<(String, String)>, LockfileError> {
            match members.iter().find(|(key, _)| key == name) {
                Some((_, value)) => object_members(value).map_err(parse_error),
                None => Ok(Vec::new()),
            }
        };
        Ok(Lockfile {
            registry_file_hashes,
            selected_yanked_versions: strings("selectedYankedVersions"),
            module_extensions: raw("moduleExtensions")?,
            facts: raw("facts")?,
            facts_versions: raw("factsVersions")?,
        })
    }

    /// The lockfile a resolution of `graph` makes of this one: the hashes of
    /// the remote registry files it read, the yanked versions it selected,
    /// and the entries of the extensions it still uses.
    pub fn updated(&self, graph: &DepGraph) -> Lockfile {
        let used = graph.extension_ids();
        let in_use = |id: &str| used.iter().any(|used| id == used || id.strip_prefix(used.as_str()).is_some_and(|rest| rest.starts_with('%')));
        let keep = |entries: &[(String, String)]| entries.iter().filter(|(id, _)| in_use(id)).cloned().collect::<Vec<_>>();
        let facts = keep(&self.facts);
        let has_facts = |id: &str| facts.iter().any(|(with_facts, _)| with_facts == id);
        Lockfile {
            registry_file_hashes: graph.registry_file_hashes.iter().filter(|(url, _)| !url.starts_with("file:")).cloned().collect(),
            selected_yanked_versions: graph.yanked.iter().cloned().collect(),
            module_extensions: keep(&self.module_extensions),
            facts_versions: self.facts_versions.iter().filter(|(id, _)| has_facts(id)).cloned().collect(),
            facts,
        }
    }

    /// The lockfile as Bazel writes it: pretty JSON, two spaces deep, the
    /// kept entries as they were written, and a newline at the end.
    pub fn render(&self) -> String {
        let quote = |text: &str| serde_json::Value::String(text.to_string()).to_string();
        let mut out = format!("{{\n  \"lockFileVersion\": {LOCKFILE_VERSION},\n");
        let hashes: Vec<(String, String)> = self
            .registry_file_hashes
            .iter()
            .map(|(url, hash)| (url.clone(), quote(hash.as_deref().unwrap_or(NOT_FOUND))))
            .collect();
        let yanked: Vec<(String, String)> = self.selected_yanked_versions.iter().map(|(key, reason)| (key.clone(), quote(reason))).collect();
        let sections = [
            ("registryFileHashes", &hashes),
            ("selectedYankedVersions", &yanked),
            ("moduleExtensions", &self.module_extensions),
            ("facts", &self.facts),
            ("factsVersions", &self.facts_versions),
        ];
        for (index, (name, entries)) in sections.iter().enumerate() {
            out.push_str(&format!("  \"{name}\": "));
            if entries.is_empty() {
                out.push_str("{}");
            } else {
                let lines: Vec<String> = entries.iter().map(|(key, value)| format!("    {}: {value}", quote(key))).collect();
                out.push_str(&format!("{{\n{}\n  }}", lines.join(",\n")));
            }
            out.push_str(if index + 1 < sections.len() { ",\n" } else { "\n" });
        }
        out.push_str("}\n");
        out
    }
}

/// The members of the top-level object of `text`, each value as written.
fn top_level_members(text: &str) -> Result<Vec<(String, String)>, String> {
    object_members(text.trim())
}

/// The members of the JSON object `text`, each value as written.
fn object_members(text: &str) -> Result<Vec<(String, String)>, String> {
    let bytes = text.as_bytes();
    let mut position = skip_space(bytes, 0);
    if bytes.get(position) != Some(&b'{') {
        return Err("expected an object".to_string());
    }
    position = skip_space(bytes, position + 1);
    let mut members = Vec::new();
    if bytes.get(position) == Some(&b'}') {
        return Ok(members);
    }
    loop {
        let key_end = value_end(bytes, position)?;
        let key: String = serde_json::from_str(&text[position..key_end]).map_err(|error| error.to_string())?;
        position = skip_space(bytes, key_end);
        if bytes.get(position) != Some(&b':') {
            return Err(format!("expected ':' after {key}"));
        }
        let value_start = skip_space(bytes, position + 1);
        let value_end = value_end(bytes, value_start)?;
        members.push((key, text[value_start..value_end].to_string()));
        position = skip_space(bytes, value_end);
        match bytes.get(position) {
            Some(b',') => position = skip_space(bytes, position + 1),
            Some(b'}') => return Ok(members),
            _ => return Err("expected ',' or '}'".to_string()),
        }
    }
}

fn skip_space(bytes: &[u8], mut position: usize) -> usize {
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    position
}

/// Where the JSON value starting at `start` ends.
fn value_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut position = start;
    let mut in_string = false;
    while let Some(&byte) = bytes.get(position) {
        position += 1;
        if in_string {
            match byte {
                b'\\' => position += 1,
                b'"' => {
                    in_string = false;
                    if depth == 0 {
                        return Ok(position);
                    }
                }
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1).ok_or("unbalanced brackets")?;
                if depth == 0 {
                    return Ok(position);
                }
            }
            b',' | b':' if depth == 0 => return Ok(position - 1),
            byte if depth == 0 && byte.is_ascii_whitespace() => return Ok(position - 1),
            _ => {}
        }
    }
    match depth == 0 && !in_string && position > start {
        true => Ok(position),
        false => Err("unexpected end of input".to_string()),
    }
}

/// The lockfile in the workspace, read through the graph, so a change to
/// it is seen. A missing lockfile is empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct LockfileKey;

impl Key for LockfileKey {
    type Value = Lockfile;
    const KIND: &'static str = "LOCKFILE";

    async fn compute(&self, ctx: &Ctx) -> Result<Lockfile, SkyError> {
        let mode = ctx.compute(&LockfileModeKey).await?;
        if *mode == LockfileMode::Off {
            return Ok(Lockfile::default());
        }
        let path = RootedPath::new(Root::Workspace, PathId::parse(LOCKFILE).expect("a valid path"));
        let Some(bytes) = bazel_fs::read_file(ctx, path).await? else {
            return Ok(Lockfile::default());
        };
        Lockfile::parse(&String::from_utf8_lossy(&bytes), *mode).map_err(|error| SkyError::new(error.to_string()))
    }
}

/// The lockfile mode of the latest command.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct LockfileModeKey;

encoded_value!(LockfileMode);

impl Key for LockfileModeKey {
    type Value = LockfileMode;
    const KIND: &'static str = "LOCKFILE_MODE";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<LockfileMode, SkyError> {
        match ctx.global::<CommandLockfileMode>() {
            Ok(mode) => Ok(mode.current()),
            Err(_) => Ok(LockfileMode::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCKFILE_TEXT: &str = r#"{
  "lockFileVersion": 28,
  "registryFileHashes": {
    "https://bcr.bazel.build/bazel_registry.json": "8a28",
    "https://bcr.bazel.build/modules/a/1.0/MODULE.bazel": "not found"
  },
  "selectedYankedVersions": {},
  "moduleExtensions": {
    "@@a+//:ext.bzl%ext": {
      "general": {
        "bzlTransitiveDigest": "x=",
        "recordedInputs": [
          "REPO_MAPPING:a+,b b+"
        ]
      }
    },
    "@@gone+//:ext.bzl%ext": {
      "general": {}
    }
  },
  "facts": {},
  "factsVersions": {}
}
"#;

    #[test]
    fn reads_what_bazel_writes() {
        let lockfile = Lockfile::parse(LOCKFILE_TEXT, LockfileMode::Update).unwrap();
        assert_eq!(lockfile.registry_file_hashes["https://bcr.bazel.build/bazel_registry.json"].as_deref(), Some("8a28"));
        assert_eq!(lockfile.registry_file_hashes["https://bcr.bazel.build/modules/a/1.0/MODULE.bazel"], None);
        assert_eq!(lockfile.module_extensions.len(), 2);
        assert!(lockfile.module_extensions[0].1.starts_with("{\n      \"general\""));
    }

    #[test]
    fn writes_what_it_reads_unchanged() {
        let lockfile = Lockfile::parse(LOCKFILE_TEXT, LockfileMode::Update).unwrap();
        assert_eq!(lockfile.render(), LOCKFILE_TEXT);
    }

    #[test]
    fn ignores_other_versions_unless_enforcing() {
        let old = LOCKFILE_TEXT.replace("\"lockFileVersion\": 28", "\"lockFileVersion\": 13");
        assert_eq!(Lockfile::parse(&old, LockfileMode::Update).unwrap(), Lockfile::default());
        assert!(matches!(Lockfile::parse(&old, LockfileMode::Error), Err(LockfileError::Version)));
        let conflicted = format!("<<<<<<< HEAD\n{LOCKFILE_TEXT}");
        let error = Lockfile::parse(&conflicted, LockfileMode::Update).unwrap_err().to_string();
        assert!(error.starts_with("Failed to read and parse the MODULE.bazel.lock file with error: "), "{error}");
        assert!(error.ends_with("This looks like a merge conflict. See https://bazel.build/external/lockfile#merge-conflicts for advice."), "{error}");
    }

    #[test]
    fn writes_an_empty_lockfile_as_bazel_does() {
        let expected = "{\n  \"lockFileVersion\": 28,\n  \"registryFileHashes\": {},\n  \"selectedYankedVersions\": {},\n  \"moduleExtensions\": {},\n  \"facts\": {},\n  \"factsVersions\": {}\n}\n";
        assert_eq!(Lockfile::default().render(), expected);
    }
}
