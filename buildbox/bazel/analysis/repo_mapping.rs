//! Repository mapping manifests: how the repositories with files in a
//! runfiles tree see each other, which runfiles libraries read to find
//! files by apparent name.

use crate::artifact::{Artifact, runfiles_repo_name};
use bazel_label::RepoName;
use bazel_loading::RepoMappingKey;
use intern::Symbol;
use skyframe::{Compute, SkyError};
use std::collections::BTreeSet;
use std::sync::Arc;

/// The repository mapping manifest of a runfiles tree of `files`, as Bazel
/// writes it: for each of `sources`, the repositories whose code might
/// look files up, a line `repo,apparent,target` for each repository with
/// files in the tree it can see, by apparent name. Consecutive
/// repositories of one module extension that see the others alike share
/// one line, `prefix+*`. Bazel's sources are those of the packages the
/// executable depends on; without them, those with files in the tree and
/// the main repository stand in.
pub async fn repo_mapping_manifest(compute: &impl Compute, files: impl IntoIterator<Item = Artifact>, sources: Option<BTreeSet<RepoName>>) -> Result<String, SkyError> {
    let with_files: BTreeSet<RepoName> = files.into_iter().map(|artifact| artifact.owner.repo()).collect();
    let sources = sources.unwrap_or_else(|| with_files.iter().copied().chain([RepoName::MAIN]).collect());
    let mut sources: Vec<RepoName> = sources.into_iter().collect();
    sources.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    sources.dedup();
    let mut mappings = Vec::with_capacity(sources.len());
    for source in &sources {
        mappings.push((*source, compute.compute(&RepoMappingKey(*source)).await?));
    }
    let mut out = String::new();
    let mut index = 0;
    while index < mappings.len() {
        let group = group_len(&mappings[index..]);
        let source = match group {
            1 => mappings[index].0.as_str().to_string(),
            _ => wildcard(mappings[index].0.as_str()),
        };
        for (apparent, target) in relevant_entries(&mappings[index].1.0.entries, &with_files) {
            out.push_str(&format!("{source},{apparent},{}\n", runfiles_repo_name(target)));
        }
        index += group;
    }
    Ok(out)
}

/// How many repositories from the first see the others alike and share
/// its name but for the last segment, as a module extension's do.
fn group_len(mappings: &[(RepoName, Arc<bazel_loading::RepoMappingValue>)]) -> usize {
    let Some((first, first_mapping)) = mappings.first() else {
        return 0;
    };
    let same = |(repo, mapping): &&(RepoName, Arc<bazel_loading::RepoMappingValue>)| mapping.0.entries == first_mapping.0.entries && same_prefix(first.as_str(), repo.as_str());
    1 + mappings[1..].iter().take_while(same).count()
}

/// Whether two canonical names agree but for their last `+` segment.
fn same_prefix(first: &str, second: &str) -> bool {
    match (first.rfind('+'), second.rfind('+')) {
        (Some(a), Some(b)) => a == b && first[..a] == second[..b],
        _ => false,
    }
}

/// A canonical name with its last segment replaced by `*`.
fn wildcard(name: &str) -> String {
    match name.rfind('+') {
        Some(separator) => format!("{}*", &name[..=separator]),
        None => name.to_string(),
    }
}

/// The entries of a mapping naming repositories with files in the tree,
/// by apparent name; the main repository's empty name is left out.
fn relevant_entries(entries: &[(Symbol, RepoName)], with_files: &BTreeSet<RepoName>) -> Vec<(Symbol, RepoName)> {
    let mut relevant: Vec<(Symbol, RepoName)> = entries.iter().filter(|(apparent, target)| !apparent.as_str().is_empty() && with_files.contains(target)).copied().collect();
    relevant.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    relevant
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_repositories_share_a_wildcard_name() {
        assert!(same_prefix("rules_rust++crate+crates__a", "rules_rust++crate+crates__b"));
        assert!(!same_prefix("rules_rust+", "rules_rust++crate+crates__b"));
        assert_eq!(wildcard("+http_archive+jemalloc"), "+http_archive+*");
    }
}
