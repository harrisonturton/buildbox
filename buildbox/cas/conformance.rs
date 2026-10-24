//! Behaviour every [`Cas`] implementation must have.
//!
//! Each implementation's tests call [`run_all`] with a function that creates
//! an empty store.

use cas::{Cas, CasError, Digest, Materialization};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Run every check against a fresh store from `new_cas`, which is given an
/// empty directory it may use.
pub async fn run_all<F, Fut>(new_cas: F)
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = Box<dyn Cas>>,
{
    check("empty_blob_is_always_present", &new_cas, empty_blob_is_always_present).await;
    check("written_blobs_can_be_read", &new_cas, written_blobs_can_be_read).await;
    check("chunked_writes_are_joined", &new_cas, chunked_writes_are_joined).await;
    check("find_missing_reports_absent_blobs", &new_cas, find_missing_reports_absent_blobs).await;
    check("reading_absent_blob_is_not_found", &new_cas, reading_absent_blob_is_not_found).await;
    check("commit_rejects_wrong_contents", &new_cas, commit_rejects_wrong_contents).await;
    check("commit_rejects_wrong_size", &new_cas, commit_rejects_wrong_size).await;
    check("uncommitted_writes_are_discarded", &new_cas, uncommitted_writes_are_discarded).await;
    check("rewriting_a_blob_succeeds", &new_cas, rewriting_a_blob_succeeds).await;
    check("put_file_stores_contents", &new_cas, put_file_stores_contents).await;
    check("materialize_creates_file", &new_cas, materialize_creates_file).await;
    check("adopted_files_are_stored", &new_cas, adopted_files_are_stored).await;
    check("materializing_absent_blob_is_not_found", &new_cas, materializing_absent_blob_is_not_found).await;
    check("materialize_all_creates_files", &new_cas, materialize_all_creates_files).await;
    check("materialize_all_of_absent_blob_is_not_found", &new_cas, materialize_all_of_absent_blob_is_not_found).await;
    check("usage_counts_blobs", &new_cas, usage_counts_blobs).await;
}

async fn check<F, Fut, T, TFut>(name: &str, new_cas: &F, test: T)
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = Box<dyn Cas>>,
    T: FnOnce(Box<dyn Cas>, PathBuf) -> TFut,
    TFut: Future<Output = ()>,
{
    // Printed so a failure can be traced to its check.
    println!("cas conformance: {name}");
    let scratch = tempfile::tempdir().unwrap();
    let cas = new_cas(scratch.path().join("cas")).await;
    test(cas, scratch.path().to_path_buf()).await;
}

async fn empty_blob_is_always_present(cas: Box<dyn Cas>, scratch: PathBuf) {
    let empty = Digest::empty();
    assert_eq!(cas.find_missing(&[empty.clone()]).await.unwrap(), vec![]);
    assert_eq!(cas.read_bytes(&empty).await.unwrap(), b"");

    let dest = scratch.join("empty");
    cas.materialize(&empty, &dest, false).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"");
}

async fn written_blobs_can_be_read(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let digest = cas.put_bytes(b"hello").await.unwrap();

    assert_eq!(digest, Digest::of_bytes(b"hello"));
    assert_eq!(cas.read_bytes(&digest).await.unwrap(), b"hello");
}

async fn chunked_writes_are_joined(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let digest = Digest::of_bytes(b"hello world");
    let mut writer = cas.write(&digest).await.unwrap();
    writer.write(b"hello").await.unwrap();
    writer.write(b"").await.unwrap();
    writer.write(b" world").await.unwrap();
    writer.commit().await.unwrap();

    assert_eq!(cas.read_bytes(&digest).await.unwrap(), b"hello world");
}

async fn find_missing_reports_absent_blobs(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let present = cas.put_bytes(b"present").await.unwrap();
    let absent_a = Digest::of_bytes(b"absent a");
    let absent_b = Digest::of_bytes(b"absent b");

    let missing = cas
        .find_missing(&[absent_b.clone(), present, absent_a.clone()])
        .await
        .unwrap();

    assert_eq!(missing, vec![absent_b, absent_a]);
}

async fn reading_absent_blob_is_not_found(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let absent = Digest::of_bytes(b"absent");

    let result = cas.read(&absent).await;

    assert!(matches!(result, Err(CasError::NotFound(digest)) if digest == absent));
}

async fn commit_rejects_wrong_contents(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let claimed = Digest::of_bytes(b"hello");
    let mut writer = cas.write(&claimed).await.unwrap();
    writer.write(b"jello").await.unwrap();

    let result = writer.commit().await;

    assert!(matches!(result, Err(CasError::DigestMismatch { .. })), "{result:?}");
    assert_eq!(cas.find_missing(&[claimed.clone()]).await.unwrap(), vec![claimed]);
}

async fn commit_rejects_wrong_size(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let actual = Digest::of_bytes(b"hello");
    let claimed = Digest::new(actual.hash(), 4).unwrap();
    let mut writer = cas.write(&claimed).await.unwrap();
    writer.write(b"hello").await.unwrap();

    let result = writer.commit().await;

    assert!(matches!(result, Err(CasError::DigestMismatch { .. })), "{result:?}");
}

async fn uncommitted_writes_are_discarded(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let digest = Digest::of_bytes(b"hello");
    let mut writer = cas.write(&digest).await.unwrap();
    writer.write(b"hello").await.unwrap();
    drop(writer);

    assert_eq!(cas.find_missing(&[digest.clone()]).await.unwrap(), vec![digest]);
}

async fn rewriting_a_blob_succeeds(cas: Box<dyn Cas>, _scratch: PathBuf) {
    let first = cas.put_bytes(b"hello").await.unwrap();
    let second = cas.put_bytes(b"hello").await.unwrap();

    assert_eq!(first, second);
    assert_eq!(cas.read_bytes(&second).await.unwrap(), b"hello");
}

async fn put_file_stores_contents(cas: Box<dyn Cas>, scratch: PathBuf) {
    let path = scratch.join("input");
    std::fs::write(&path, b"from a file").unwrap();

    let digest = cas.put_file(&path).await.unwrap();

    assert_eq!(digest, Digest::of_bytes(b"from a file"));
    assert_eq!(cas.read_bytes(&digest).await.unwrap(), b"from a file");
}

async fn materialize_creates_file(cas: Box<dyn Cas>, scratch: PathBuf) {
    let digest = cas.put_bytes(b"#!/bin/sh").await.unwrap();
    let plain = scratch.join("plain");
    let executable = scratch.join("executable");

    cas.materialize(&digest, &plain, false).await.unwrap();
    cas.materialize(&digest, &executable, true).await.unwrap();

    assert_eq!(std::fs::read(&plain).unwrap(), b"#!/bin/sh");
    assert_eq!(std::fs::read(&executable).unwrap(), b"#!/bin/sh");
    assert!(!is_executable(&plain), "plain file is executable");
    assert!(is_executable(&executable), "executable file isn't executable");
}

async fn adopted_files_are_stored(cas: Box<dyn Cas>, scratch: PathBuf) {
    let plain = scratch.join("plain");
    let executable = scratch.join("executable");
    std::fs::write(&plain, b"plain output").unwrap();
    std::fs::write(&executable, b"executable output").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

    let plain_digest = cas.adopt_file(&plain, false).await.unwrap();
    let executable_digest = cas.adopt_file(&executable, true).await.unwrap();

    assert_eq!(plain_digest, Digest::of_bytes(b"plain output"));
    assert_eq!(executable_digest, Digest::of_bytes(b"executable output"));
    let digests = [plain_digest.clone(), executable_digest.clone()];
    assert_eq!(cas.find_missing(&digests).await.unwrap(), vec![]);
    assert_eq!(cas.read_bytes(&executable_digest).await.unwrap(), b"executable output");
    // Each can be materialized either way.
    for (index, digest) in digests.iter().enumerate() {
        let as_plain = scratch.join(format!("as-plain-{index}"));
        let as_executable = scratch.join(format!("as-executable-{index}"));
        cas.materialize(digest, &as_plain, false).await.unwrap();
        cas.materialize(digest, &as_executable, true).await.unwrap();
        assert!(!is_executable(&as_plain), "{}", as_plain.display());
        assert!(is_executable(&as_executable), "{}", as_executable.display());
        assert_eq!(std::fs::read(&as_plain).unwrap(), std::fs::read(&as_executable).unwrap());
    }
}

async fn materializing_absent_blob_is_not_found(cas: Box<dyn Cas>, scratch: PathBuf) {
    let absent = Digest::of_bytes(b"absent");
    let dest = scratch.join("dest");

    let result = cas.materialize(&absent, &dest, false).await;

    assert!(matches!(result, Err(CasError::NotFound(digest)) if digest == absent));
    assert!(!dest.exists());
}

async fn materialize_all_creates_files(cas: Box<dyn Cas>, scratch: PathBuf) {
    let mut files = Vec::new();
    // More files than a store might create at once.
    for index in 0..600 {
        let contents = format!("file {index}");
        let digest = cas.put_bytes(contents.as_bytes()).await.unwrap();
        files.push(Materialization {
            digest,
            dest: scratch.join(format!("file-{index}")),
            executable: index % 2 == 0,
        });
    }

    cas.materialize_all(files.clone()).await.unwrap();

    for (index, file) in files.iter().enumerate() {
        assert_eq!(std::fs::read(&file.dest).unwrap(), format!("file {index}").as_bytes());
        assert_eq!(is_executable(&file.dest), file.executable, "{}", file.dest.display());
    }
}

async fn materialize_all_of_absent_blob_is_not_found(cas: Box<dyn Cas>, scratch: PathBuf) {
    let present = cas.put_bytes(b"present").await.unwrap();
    let absent = Digest::of_bytes(b"absent");
    let files = vec![
        Materialization {
            digest: present,
            dest: scratch.join("present"),
            executable: false,
        },
        Materialization {
            digest: absent.clone(),
            dest: scratch.join("absent"),
            executable: false,
        },
    ];

    let result = cas.materialize_all(files).await;

    assert!(matches!(result, Err(CasError::NotFound(digest)) if digest == absent));
    assert!(!scratch.join("absent").exists());
}

async fn usage_counts_blobs(cas: Box<dyn Cas>, scratch: PathBuf) {
    let before = cas.usage().await.unwrap();

    cas.put_bytes(b"hello").await.unwrap();
    cas.put_bytes(b"hello").await.unwrap();
    cas.put_bytes(b"world!").await.unwrap();
    // An executable, which is counted once however it's materialized.
    let tool = scratch.join("tool");
    std::fs::write(&tool, b"tool").unwrap();
    let tool = cas.adopt_file(&tool, true).await.unwrap();
    cas.materialize(&tool, &scratch.join("plain-tool"), false).await.unwrap();

    let after = cas.usage().await.unwrap();
    assert_eq!(after.blobs - before.blobs, 3);
    assert_eq!(after.bytes - before.bytes, 15);
}

fn is_executable(path: &Path) -> bool {
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    mode & 0o111 != 0
}
