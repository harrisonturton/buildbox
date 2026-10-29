//! Tests that create real containers. They need the helper binary, given by
//! the CONTAINER_INIT environment variable, and a machine that allows
//! unprivileged user namespaces; elsewhere they pass without checking
//! anything, saying why.

use super::*;
use container::Mount;
use std::os::unix::fs::MetadataExt;

/// A runtime and a root filesystem to run containers with, or `None` if
/// this machine can't run them.
struct Fixture {
    runtime: NamespaceRuntime,
    rootfs: PathBuf,
    workspace: PathBuf,
    host_mounts: Vec<Mount>,
    _dir: TempDir,
}

async fn fixture() -> Option<Fixture> {
    let helper = std::env::var_os("CONTAINER_INIT").expect("CONTAINER_INIT isn't set");
    // Absolute and resolved, so it is found from the helper's working
    // directory and matches AppArmor profiles by path.
    let program = std::fs::canonicalize(helper).unwrap();
    let helper = Helper {
        program,
        args: vec![],
    };

    let dir = tempfile::tempdir().unwrap();
    let runtime = match NamespaceRuntime::open(helper, &dir.path().join("state")).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("skipping: this machine can't run containers: {error}");
            return None;
        }
    };

    let rootfs = dir.path().join("rootfs");
    let host_mounts = borrow_host_usr(&rootfs);
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    Some(Fixture {
        runtime,
        rootfs,
        workspace,
        host_mounts,
        _dir: dir,
    })
}

/// Make a root filesystem that uses the host's programs: the host's `/usr`
/// is mounted in it, and the top-level directories that link into `/usr`
/// on the host link there in it too.
fn borrow_host_usr(rootfs: &Path) -> Vec<Mount> {
    std::fs::create_dir_all(rootfs.join("usr")).unwrap();
    for name in ["bin", "lib", "lib32", "lib64", "sbin"] {
        let host = Path::new("/").join(name);
        if let Ok(target) = std::fs::read_link(&host) {
            std::os::unix::fs::symlink(target, rootfs.join(name)).unwrap();
        }
    }
    vec![Mount {
        source: PathBuf::from("/usr"),
        target: PathBuf::from("/usr"),
    }]
}

impl Fixture {
    fn spec(&self, script: &str) -> ContainerSpec {
        let mut mounts = self.host_mounts.clone();
        mounts.push(Mount {
            source: self.workspace.clone(),
            target: PathBuf::from("/workspace"),
        });
        ContainerSpec {
            rootfs: self.rootfs.clone(),
            mounts,
            args: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
            env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
            working_dir: PathBuf::from("/workspace"),
            network: Network::Isolated,
        }
    }

    async fn run(&self, script: &str) -> ProcessOutput {
        self.runtime.run(&self.spec(script), Some(Duration::from_secs(10))).await.unwrap()
    }
}

fn stdout(output: &ProcessOutput) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn runs_commands_as_root_in_their_own_namespaces() {
    let Some(f) = fixture().await else { return };

    let output = f.run("echo $$ $(id -u) $(hostname) $PWD; echo oops >&2; exit 3").await;

    // The command is PID 2: the helper's init process is PID 1.
    assert_eq!(stdout(&output), "2 0 buildbox /workspace\n");
    assert_eq!(output.stderr, b"oops\n");
    assert_eq!(output.exit_code, 3);
}

#[tokio::test]
async fn the_root_is_read_only_and_mounts_are_writable() {
    let Some(f) = fixture().await else { return };

    let output = f.run("touch /new 2>/dev/null || echo read-only; echo hi > out; touch /tmp/x && echo tmp").await;

    assert_eq!(stdout(&output), "read-only\ntmp\n");
    let out = f.workspace.join("out");
    assert_eq!(std::fs::read(&out).unwrap(), b"hi\n");
    // Root in the container is the user running buildbox.
    // SAFETY: getuid takes no arguments and can't fail.
    assert_eq!(std::fs::metadata(&out).unwrap().uid(), unsafe { libc::getuid() });
    assert!(!f.rootfs.join("new").exists());
}

#[tokio::test]
async fn isolated_containers_only_have_loopback() {
    let Some(f) = fixture().await else { return };

    let output = f.run("tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '").await;

    assert_eq!(stdout(&output), "lo\n");
}

#[tokio::test]
async fn processes_left_running_are_killed() {
    let Some(f) = fixture().await else { return };

    // setsid takes `sleep` out of the process group, but not the container.
    let output = f.run("setsid sleep 300 & echo started").await;

    assert_eq!(stdout(&output), "started\n");
}

#[tokio::test]
async fn stops_containers_that_time_out() {
    let Some(f) = fixture().await else { return };
    let timeout = Duration::from_millis(300);

    let result = f.runtime.run(&f.spec("sleep 300"), Some(timeout)).await;

    assert!(matches!(result, Err(ContainerError::Timeout(t)) if t == timeout), "{result:?}");
}

#[tokio::test]
async fn reports_programs_that_cant_start() {
    let Some(f) = fixture().await else { return };
    let mut spec = f.spec("");
    spec.args = vec!["no-such-program".to_string()];

    let result = f.runtime.run(&spec, None).await;

    assert!(matches!(&result, Err(ContainerError::Start(message)) if message.contains("no-such-program")), "{result:?}");
}

#[tokio::test]
async fn refuses_to_mount_through_symlinks_in_the_image() {
    let Some(f) = fixture().await else { return };
    std::os::unix::fs::symlink("/etc", f.rootfs.join("workspace")).unwrap();

    let result = f.runtime.run(&f.spec("true"), None).await;

    assert!(matches!(result, Err(ContainerError::Setup(_))), "{result:?}");
}

#[tokio::test]
async fn keeps_interactive_containers_running_to_talk_to() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let Some(f) = fixture().await else { return };

    let mut running = f.runtime.start(&f.spec("while read line; do echo \"got $line\"; done")).await.unwrap();
    let mut stdout = BufReader::new(&mut running.process.stdout);
    for line in ["one", "two"] {
        running.process.stdin.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        running.process.stdin.flush().await.unwrap();
        let mut answer = String::new();
        stdout.read_line(&mut answer).await.unwrap();
        assert_eq!(answer, format!("got {line}\n"));
    }
}
