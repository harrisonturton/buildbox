//! Every bb server running on this machine for this user, whatever config
//! started it: those in the registry servers record themselves in, and
//! servers too old to, found among the processes.

use crate::format;
use bazel_rust_connect::buildbox::bazel::v1::{BazelClient, GetServerRequest, GetServerResponse};
use connectrpc::client::{ClientConfig, HttpClient};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

/// How long a server may take to describe itself before it's listed
/// without the description.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What the list shows for what it can't tell.
const UNKNOWN: &str = "?";

/// The heading of [`line`]'s rows.
pub const HEADING: &str = "PID       NAME                STATE     UPTIME    COMMANDS  URL                       DIRECTORY";

/// A running server, as the machine knows it.
pub struct Found {
    pub pid: u32,
    /// Where it listens, if that's known.
    pub address: Option<SocketAddr>,
    /// Its name: [`DEFAULT_NAME`](crate::daemon::DEFAULT_NAME) for the
    /// default server of its config, or `?` if it didn't register.
    pub name: String,
    /// Where it keeps its state, if it registered.
    pub state_dir: Option<PathBuf>,
}

/// Every server running for this user, by pid.
pub fn find() -> Vec<Found> {
    let mut found: Vec<Found> = registry::running(&registry::default_dir())
        .into_iter()
        .map(|entry| Found {
            pid: entry.pid,
            address: Some(entry.address),
            name: entry.name.unwrap_or_else(|| crate::daemon::DEFAULT_NAME.to_string()),
            state_dir: Some(entry.state_dir),
        })
        .collect();
    let registered: HashSet<u32> = found.iter().map(|server| server.pid).collect();
    found.extend(unregistered(&registered));
    found.sort_by_key(|server| server.pid);
    found
}

/// The servers of this user's that aren't in the registry, with the
/// address each listens on, if it can be told.
fn unregistered(registered: &HashSet<u32>) -> Vec<Found> {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let sockets = listening_sockets();
    let mut found = Vec::new();
    for process in processes.flatten() {
        let Some(pid) = process.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let owned = process.metadata().is_ok_and(|metadata| metadata.uid() == uid);
        if registered.contains(&pid) || !owned || !crate::daemon::is_server_process(pid) {
            continue;
        }
        found.push(Found {
            pid,
            address: listening_address(pid, &sockets),
            name: UNKNOWN.to_string(),
            state_dir: None,
        });
    }
    found
}

/// The address process `pid` listens on, of `sockets`, the machine's
/// listening TCP sockets by inode.
pub fn listening_address(pid: u32, sockets: &HashMap<u64, SocketAddr>) -> Option<SocketAddr> {
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    for fd in fds.flatten() {
        let Ok(target) = std::fs::read_link(fd.path()) else {
            continue;
        };
        let inode = target.to_str().and_then(|text| text.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok());
        if let Some(address) = inode.and_then(|inode| sockets.get(&inode)) {
            return Some(*address);
        }
    }
    None
}

/// The listening TCP sockets, over IPv4 and IPv6, by inode.
pub fn listening_sockets() -> HashMap<u64, SocketAddr> {
    /// The state of a listening socket in `/proc/net/tcp`.
    const LISTEN: &str = "0A";
    let mut sockets = HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) = (fields.get(1), fields.get(3), fields.get(9)) else {
                continue;
            };
            if *state != LISTEN {
                continue;
            }
            if let (Some(address), Ok(inode)) = (parse_proc_address(local), inode.parse::<u64>()) {
                sockets.insert(inode, address);
            }
        }
    }
    sockets
}

/// An address as `/proc/net/tcp*` gives it: the IP in hex, each 32-bit
/// word in host order, then `:` and the port in hex.
fn parse_proc_address(text: &str) -> Option<SocketAddr> {
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words: Vec<u32> = (0..ip.len() / 8).map(|index| u32::from_str_radix(&ip[index * 8..index * 8 + 8], 16)).collect::<Result<_, _>>().ok()?;
    let ip = match words.as_slice() {
        [word] => IpAddr::V4(Ipv4Addr::from(word.to_ne_bytes())),
        [a, b, c, d] => {
            let bytes: Vec<u8> = [a, b, c, d].iter().flat_map(|word| word.to_ne_bytes()).collect();
            let bytes: [u8; 16] = bytes.try_into().ok()?;
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// What the server at `address` says of itself, unless it doesn't answer
/// in time.
pub async fn describe(address: SocketAddr) -> Option<GetServerResponse> {
    let uri = crate::client::url_for(address).parse().ok()?;
    let client = BazelClient::new(HttpClient::plaintext(), ClientConfig::new(uri));
    let described = tokio::time::timeout(DESCRIBE_TIMEOUT, client.get_server(GetServerRequest::default())).await;
    Some(described.ok()?.ok()?.into_owned())
}

/// A row of `bb server list --all`: pid, name, state, uptime, commands,
/// URL and where the server keeps its state.
pub async fn line(server: &Found) -> String {
    let name = &server.name;
    let url = server.address.map(crate::client::url_for).unwrap_or_else(|| UNKNOWN.to_string());
    let state_dir = server.state_dir.as_ref().map_or(UNKNOWN.to_string(), |dir| dir.display().to_string());
    let described = match server.address {
        Some(address) => describe(address).await,
        None => None,
    };
    let Some(described) = described else {
        return format!("{:<10}{name:<20}{:<10}{UNKNOWN:<10}{UNKNOWN:<10}{url:<26}{state_dir}", server.pid, "running");
    };
    let state = if described.shutting_down { "stopping" } else { "running" };
    let uptime = format::duration(Duration::from_secs(described.uptime_seconds));
    format!("{:<10}{name:<20}{state:<10}{uptime:<10}{:<10}{url:<26}{state_dir}", server.pid, described.commands.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_addresses_are_parsed() {
        assert_eq!(parse_proc_address("0100007F:C383"), Some("127.0.0.1:50051".parse().unwrap()));
        assert_eq!(parse_proc_address("00000000000000000000000001000000:C383"), Some("[::1]:50051".parse().unwrap()));
        assert_eq!(parse_proc_address("nonsense"), None);
    }
}
