//! The local servers: finding one, starting it in the background, and
//! stopping it. The default server listens at the configured address; a
//! named server listens wherever it could, which it records in its address
//! file.

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

/// The name the default server is listed by, which selects it too.
pub const DEFAULT_NAME: &str = config::DEFAULT_SERVER_NAME;

/// One of the servers the configuration describes.
pub struct Daemon {
    /// `None` for the default server.
    name: Option<String>,
    listen: SocketAddr,
    address_file: PathBuf,
    bazel_dir: PathBuf,
    /// The config file given, which a server started from here reads too.
    config_file: Option<PathBuf>,
}

impl Daemon {
    /// The server named `name`, or `$BB_SERVER_NAME`, or the default one,
    /// as the config file at `config_file`, or the default file, describes
    /// it.
    pub fn new(config_file: Option<&Path>, name: Option<&str>) -> Result<Daemon, Box<dyn Error>> {
        let settings = Settings {
            server_name: name.map(str::to_string),
            ..Settings::default()
        };
        let config = config::load(config_file, settings, &Environment::from_process())?;
        Ok(Daemon {
            name: config.name,
            listen: config.listen,
            address_file: config.address_file,
            bazel_dir: config.bazel_dir,
            config_file: config_file.map(Path::to_path_buf),
        })
    }

    /// Every server: the default one, then those with names that have
    /// stored anything, by name.
    pub fn all(config_file: Option<&Path>) -> Result<Vec<Daemon>, Box<dyn Error>> {
        let config = config::load(config_file, Settings::default(), &Environment::from_process())?;
        let names = config.servers_dir.as_deref().map(config::named_servers).unwrap_or_default();
        let mut daemons = vec![Daemon::new(config_file, Some(DEFAULT_NAME))?];
        for name in names {
            daemons.push(Daemon::new(config_file, Some(&name))?);
        }
        Ok(daemons)
    }

    /// What the server is called: its name, or [`DEFAULT_NAME`].
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(DEFAULT_NAME)
    }

    /// A client for the server's Bazel service.
    pub fn client(&self) -> Result<BazelClient<HttpClient>, Box<dyn Error>> {
        let uri = self.url()?.parse()?;
        Ok(BazelClient::new(HttpClient::plaintext(), ClientConfig::new(uri)))
    }

    /// The URL to reach the server at.
    pub fn url(&self) -> Result<String, Box<dyn Error>> {
        match self.address() {
            Some(address) => Ok(format!("http://{address}")),
            None => Err(format!("the {PRODUCT} server '{}' isn't running", self.name()).into()),
        }
    }

    /// Where a server started from here writes its log.
    pub fn log_path(&self) -> PathBuf {
        self.bazel_dir.join("server.log")
    }

    /// Whether the server is listening.
    pub async fn is_running(&self) -> bool {
        let Some(address) = self.address() else {
            return false;
        };
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
        Err(format!("the {PRODUCT} server '{}' didn't start listening within {START_TIMEOUT:?}; see {}", self.name(), self.log_path().display()).into())
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
        let port = self.address().map_or(self.listen.port(), |address| address.port());
        let Some(pid) = listening_pid(port) else {
            return Err(format!("the server can't be asked to stop ({why}), and no process of yours is listening on port {port}").into());
        };
        if !is_server_process(pid) {
            return Err(format!("the process listening on port {port} (pid {pid}) isn't a {PRODUCT} server").into());
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
        if let Some(name) = &self.name {
            command.arg("--server-name").arg(name);
        }
        command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0);
        command.spawn()?;
        Ok(())
    }

    /// The address to reach the server at, if it can be known: for a named
    /// server, as its address file records, if the server that wrote it is
    /// still running; for the default server, the configured address, or
    /// loopback if that's every interface.
    fn address(&self) -> Option<SocketAddr> {
        if self.name.is_none() {
            let url = crate::client::url_for(self.listen);
            return Some(url.trim_start_matches("http://").parse().unwrap_or(self.listen));
        }
        let recorded = std::fs::read_to_string(&self.address_file).ok()?;
        let mut lines = recorded.lines();
        let address: SocketAddr = lines.next()?.parse().ok()?;
        let pid: u32 = lines.next()?.parse().ok()?;
        process_exists(pid).then_some(address)
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
