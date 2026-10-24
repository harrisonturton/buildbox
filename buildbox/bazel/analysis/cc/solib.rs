//! The `_solib_<cpu>` directory: symlinks to the shared libraries binaries
//! link against, gathered in one directory so that they need few `-rpath`s,
//! named as Bazel names them.

use crate::action::{Action, ActionKind};
use crate::artifact::Artifact;
use crate::inputs::InputSet;
use crate::state::RuleState;
use bazel_label::Label;
use data_encoding::HEXLOWER;
use intern::Symbol;

/// The longest file name a symlink may have before it's hashed.
const MAX_FILENAME_LENGTH: usize = 255;

/// The soname a shared library at `path`, relative to its root, gets in
/// the configuration whose output directory is `mnemonic`: its own name if
/// `preserve_name`, or one mangled from its path.
pub fn soname(path: &str, preserve_name: bool, mnemonic: &str) -> String {
    if preserve_name {
        return path.rsplit('/').next().unwrap_or(path).to_string();
    }
    let mnemonic_mangling = match mnemonic.find("ST-") {
        Some(start) => format!("{}_", &mnemonic[start..]),
        None => String::new(),
    };
    format!("lib{mnemonic_mangling}{}", escaped_path(path))
}

/// Symlink `library` into `solib_directory`, under a name mangled from its
/// path and, if `prefix_consumer`, the rule's label.
pub fn library_symlink(state: &RuleState, library: Artifact, solib_directory: &str, preserve_name: bool, prefix_consumer: bool) -> anyhow::Result<Artifact> {
    let path = root_relative(&library);
    let name = mangled_name(state.label, solib_directory, &state.config.output_directory, &path, preserve_name, prefix_consumer);
    symlink(state, library, &name)
}

/// Symlink a C++ runtime library into the solib directory under its own
/// name.
pub fn runtime_symlink(state: &RuleState, library: Artifact, solib_directory: &str) -> anyhow::Result<Artifact> {
    let name = format!("{solib_directory}/{}", library.exec_path.name());
    symlink(state, library, &name)
}

/// Register the action linking `name`, relative to the output directory,
/// to `library`.
fn symlink(state: &RuleState, library: Artifact, name: &str) -> anyhow::Result<Artifact> {
    let path = root_relative(&library);
    if path.starts_with("_solib_") {
        anyhow::bail!("Library '{path}' must not have a root-relative path that starts with _solib_.");
    }
    let link = state.declare_at_root(name)?;
    state.register(Action {
        mnemonic: Symbol::intern("SolibSymlink"),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![link],
        kind: ActionKind::Symlink {
            target: library,
            executable: false,
        },
    })?;
    Ok(link)
}

fn mangled_name(label: Label, solib_directory: &str, mnemonic: &str, path: &str, preserve_name: bool, prefix_consumer: bool) -> String {
    let escaped_rule_path = escaped_path(&format!("_{label}"));
    let soname = soname(path, preserve_name, mnemonic);
    if !preserve_name {
        let filename = match prefix_consumer {
            true => format!("{escaped_rule_path}__{soname}"),
            false => soname,
        };
        return format!("{solib_directory}/{}", hash_if_too_long(&filename));
    }
    let directory = path.rsplit_once('/').map(|(directory, _)| directory).unwrap_or("");
    let escaped_library_path = escaped_path(&format!("_{directory}"));
    let escaped_full_path = match prefix_consumer {
        true => format!("{escaped_rule_path}__{escaped_library_path}"),
        false => escaped_library_path,
    };
    format!("{solib_directory}/{}/{soname}", hash_if_too_long(&escaped_full_path))
}

/// `path` with the characters that can't be in one file name escaped, as
/// Bazel's `Actions.escapedPath` escapes them.
pub fn escaped_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        match character {
            '_' => escaped.push_str("_U"),
            '/' => escaped.push_str("_S"),
            '\\' => escaped.push_str("_B"),
            ':' => escaped.push_str("_C"),
            '@' => escaped.push_str("_A"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// `filename`, or if it's too long to be one, its SHA-256 with its
/// extension.
pub fn hash_if_too_long(filename: &str) -> String {
    if filename.len() <= MAX_FILENAME_LENGTH {
        return filename.to_string();
    }
    let hashed = HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, filename.as_bytes()).as_ref());
    match filename.rsplit_once('.').map(|(_, extension)| extension).filter(|extension| !extension.is_empty() && !extension.contains('/')) {
        Some(extension) => format!("{hashed}.{extension}"),
        None => hashed,
    }
}

fn root_relative(artifact: &Artifact) -> String {
    artifact.exec_path.strip_prefix(artifact.root()).unwrap_or(artifact.exec_path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonames_are_mangled_from_paths() {
        assert_eq!(soname("external/a+/libx.so", false, "k8-fastbuild"), "libexternal_Sa+_Slibx.so");
        assert_eq!(soname("pkg/libx.so", false, "k8-opt-exec-ST-abc"), "libST-abc_pkg_Slibx.so");
        assert_eq!(soname("pkg/libx.so", true, "k8-fastbuild"), "libx.so");
    }

    #[test]
    fn mangled_names_can_be_prefixed_by_their_consumer() {
        let label = Label::parse_canonical("@@//pkg:lib").unwrap();
        assert_eq!(mangled_name(label, "_solib_k8", "k8-fastbuild", "pkg/libx.so", true, true), "_solib_k8/_U_S_Spkg_Clib___Upkg/libx.so");
        assert_eq!(mangled_name(label, "_solib_k8", "k8-fastbuild", "pkg/libx.so", false, false), "_solib_k8/libpkg_Slibx.so");
    }

    #[test]
    fn long_names_are_hashed_keeping_their_extension() {
        let long = format!("{}.so", "x".repeat(300));
        let hashed = hash_if_too_long(&long);
        assert_eq!(hashed.len(), 64 + 3);
        assert!(hashed.ends_with(".so"));
    }
}
