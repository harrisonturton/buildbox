//! The local server: finding it, starting it in the background, and
//! stopping it.

use bazel_rust_connect::buildbox::bazel::v1::{BazelClient, GetServerRequest, ShutdownRequest};
use bazel_server::{PRODUCT, VERSION};
use config::{Environment, Settings};
use connectrpc::client::{ClientConfig, HttpClient};
use std::error::Error;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How long to wait for a server this command started to begin listening.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connection attempt may take before the server is presumed
/// not to be running.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a server that's stopping is waited for before saying so.
const QUIET_WAIT: Duration = Duration::from_secs(1);

/// How often a server that's starting or stopping is checked on.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The server the configuration names.
pub struct Daemon {
    listen: SocketAddr,
    bazel_dir: PathBuf,
    /// The config file given, which a server started from here reads too.
    config_file: Option<PathBuf>,
}

impl Daemon {
    /// The server the config file at `config_file`, or the default one,
    /// describes.
    pub fn new(config_file: Option<&Path>) -> Result<Daemon, Box<dyn Error>> {
        let config = config::load(config_file, Settings::default(), &Environment::from_process())?;
        Ok(Daemon {
            listen: config.listen,
            bazel_dir: config.bazel_dir,
            config_file: config_file.map(Path::to_path_buf),
        })
    }

    /// A client for the server's Bazel service.
    pub fn client(&self) -> Result<BazelClient<HttpClient>, Box<dyn Error>> {
        let uri = crate::client::url_for(self.listen).parse()?;
        Ok(BazelClient::new(HttpClient::plaintext(), ClientConfig::new(uri)))
    }

    /// Where a server started from here writes its log.
    pub fn log_path(&self) -> PathBuf {
        self.bazel_dir.join("server.log")
    }

    /// Whether a server is listening.
    pub async fn is_running(&self) -> bool {
        let address = self.address();
        matches!(tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(address)).await, Ok(Ok(_)))
    }

    /// Start the server in the background, unless it's already listening.
    pub async fn ensure_running(&self) -> Result<(), Box<dyn Error>> {
        if self.is_running().await {
            return Ok(());
        }
        self.start()?;
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if self.is_running().await {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Err(format!("the server didn't start listening on {} within {START_TIMEOUT:?}", self.address()).into())
    }

    /// Ask the server to stop, once its commands have finished or
    /// cancelling them, and wait until its process has exited. A server
    /// that can't be asked, being too old or not answering, is sent SIGTERM,
    /// which cancels its commands.
    pub async fn stop(&self, cancel_commands: bool) -> Result<(), Box<dyn Error>> {
        if !self.is_running().await {
            return Ok(());
        }
        let client = self.client()?;
        let pid = match client.get_server(GetServerRequest::default()).await {
            Ok(server) => {
                let pid = server.into_owned().pid;
                let request = ShutdownRequest {
                    cancel_commands,
                    ..Default::default()
                };
                client.shutdown(request).await?;
                pid
            }
            Err(error) => self.terminate(&error.to_string())?,
        };
        let started = Instant::now();
        let mut said = false;
        while process_exists(pid) {
            if !said && started.elapsed() > QUIET_WAIT {
                eprintln!("Waiting for the running {PRODUCT} server to finish its commands and stop...");
                said = true;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(())
    }

    /// Send SIGTERM to the server listening on the configured port, which
    /// couldn't be asked to stop because of `why`, returning its pid.
    fn terminate(&self, why: &str) -> Result<u32, Box<dyn Error>> {
        let Some(pid) = listening_pid(self.listen.port()) else {
            return Err(format!("the server can't be asked to stop ({why}), and no process of yours is listening on port {}", self.listen.port()).into());
        };
        if !is_server_process(pid) {
            return Err(format!("the process listening on port {} (pid {pid}) isn't a {PRODUCT} server", self.listen.port()).into());
        }
        // SAFETY: kill only sends a signal to the process.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            return Err(format!("failed to send SIGTERM to pid {pid}: {}", std::io::Error::last_os_error()).into());
        }
        Ok(pid)
    }

    /// Run `bb up` detached from this process, logging to the bazel
    /// directory.
    fn start(&self) -> Result<(), Box<dyn Error>> {
        std::fs::create_dir_all(&self.bazel_dir)?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.log_path())?;
        let program = std::env::current_exe()?;
        eprintln!("Starting local {PRODUCT} server ({VERSION}) and connecting to it...");
        let mut command = std::process::Command::new(program);
        command.arg("up");
        if let Some(config_file) = &self.config_file {
            command.arg("--config").arg(config_file);
        }
        command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0);
        command.spawn()?;
        Ok(())
    }

    /// The address to reach the server at: loopback, if it listens on
    /// every interface.
    fn address(&self) -> SocketAddr {
        crate::client::url_for(self.listen)
            .trim_start_matches("http://")
            .parse()
            .unwrap_or(self.listen)
    }
}

/// Whether the process `pid` is running: it exists and hasn't exited.
fn process_exists(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next());
    !matches!(state, Some("Z" | "X"))
}

/// The pid of a process of this user's listening on TCP `port`, found by
/// matching the socket's inode against each process's open files.
fn listening_pid(port: u16) -> Option<u32> {
    let inodes = listening_inodes(port);
    if inodes.is_empty() {
        return None;
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let inode = target.to_str().and_then(|text| text.strip_prefix("socket:[")?.strip_suffix(']').map(str::to_string));
            if inode.is_some_and(|inode| inodes.contains(&inode)) {
                return Some(pid);
            }
        }
    }
    None
}

/// The inodes of the sockets listening on TCP `port`, over IPv4 or IPv6.
fn listening_inodes(port: u16) -> Vec<String> {
    /// The state of a listening socket in `/proc/net/tcp`.
    const LISTEN: &str = "0A";
    let port = format!("{port:04X}");
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) = (fields.get(1), fields.get(3), fields.get(9)) else {
                continue;
            };
            if local.rsplit(':').next() == Some(port.as_str()) && *state == LISTEN {
                inodes.push(inode.to_string());
            }
        }
    }
    inodes
}

/// Whether the process `pid` is a server: `bb up`, by whatever name bb was
/// run.
fn is_server_process(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
    args.get(1) == Some(&&b"up"[..])
}
