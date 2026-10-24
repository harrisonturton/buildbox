//! Safe wrappers over the Linux system calls used to build containers.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// What `fork` returned in this process.
pub enum Fork {
    Parent { child: libc::pid_t },
    Child,
}

pub fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other(format!("{} contains NUL", path.display())))
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(result)
}

pub fn unshare(flags: libc::c_int) -> io::Result<()> {
    // SAFETY: unshare takes no pointers.
    check(unsafe { libc::unshare(flags) })?;
    Ok(())
}

/// Fork this process. Only call this from a single-threaded process, so the
/// child can safely do anything the parent could.
pub fn fork() -> io::Result<Fork> {
    // SAFETY: fork takes no arguments. The caller is single-threaded, so no
    // lock can be held by a thread that doesn't exist in the child.
    let pid = check(unsafe { libc::fork() })?;
    if pid == 0 {
        return Ok(Fork::Child);
    }
    Ok(Fork::Parent { child: pid })
}

/// Wait for a child to change state, retrying if interrupted. `pid` is as
/// for waitpid: -1 waits for any child. Returns the child and its status.
pub fn wait(pid: libc::pid_t) -> io::Result<(libc::pid_t, libc::c_int)> {
    loop {
        let mut status = 0;
        // SAFETY: `status` is a valid pointer for the duration of the call.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result != -1 {
            return Ok((result, status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The exit code for a wait status, or 128 plus the signal for a process
/// killed by one, as shells report it.
pub fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        return libc::WEXITSTATUS(status);
    }
    if libc::WIFSIGNALED(status) {
        return 128 + libc::WTERMSIG(status);
    }
    -1
}

/// Kill this process if its parent dies.
pub fn die_with_parent() -> io::Result<()> {
    // SAFETY: PR_SET_PDEATHSIG takes a signal number and no pointers.
    check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) })?;
    Ok(())
}

pub fn mount(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let source = source.map(path_cstring).transpose()?;
    let target = path_cstring(target)?;
    let fstype = fstype.map(|fstype| CString::new(fstype).map_err(io::Error::other)).transpose()?;
    let data = data.map(|data| CString::new(data).map_err(io::Error::other)).transpose()?;

    let as_ptr = |value: &Option<CString>| value.as_ref().map_or(std::ptr::null(), |value| value.as_ptr());
    // SAFETY: every pointer is either null or a NUL-terminated string that
    // lives until the call returns.
    check(unsafe {
        libc::mount(
            as_ptr(&source),
            target.as_ptr(),
            as_ptr(&fstype),
            flags,
            as_ptr(&data).cast(),
        )
    })?;
    Ok(())
}

/// The mount flags of the filesystem at `path` that a mount in a user
/// namespace can't remove, and so must keep when remounting.
pub fn locked_mount_flags(path: &Path) -> io::Result<libc::c_ulong> {
    let path = path_cstring(path)?;
    // SAFETY: statvfs is plain data, valid when zeroed.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is valid for writes.
    check(unsafe { libc::statvfs(path.as_ptr(), &mut stat) })?;

    let pairs = [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ];
    let mut flags = 0;
    for (stat_flag, mount_flag) in pairs {
        if stat.f_flag & stat_flag != 0 {
            flags |= mount_flag;
        }
    }
    Ok(flags)
}

/// Make the current directory the root, and detach the old root.
pub fn pivot_root_to_current_dir() -> io::Result<()> {
    let dot = c".";
    // SAFETY: both paths are NUL-terminated static strings.
    check(unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) } as libc::c_int)?;
    // After pivot_root(".", "."), the old root is mounted on top of the new
    // one, at the current directory.
    // SAFETY: `dot` is NUL-terminated.
    check(unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) })?;
    std::env::set_current_dir("/")
}

pub fn set_hostname(name: &str) -> io::Result<()> {
    // SAFETY: the pointer and length describe `name`'s bytes.
    check(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) })?;
    Ok(())
}

/// Bring up the loopback interface in this network namespace.
pub fn bring_up_loopback() -> io::Result<()> {
    // The parts of `struct ifreq` used to get and set interface flags.
    #[repr(C)]
    struct InterfaceFlagsRequest {
        name: [u8; libc::IFNAMSIZ],
        flags: libc::c_short,
        _rest_of_union: [u8; 22],
    }

    // SAFETY: socket takes no pointers.
    let socket = check(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: `socket` was just opened and nothing else owns it.
    let socket = unsafe { OwnedFd::from_raw_fd(socket) };

    let mut request = InterfaceFlagsRequest {
        name: [0; libc::IFNAMSIZ],
        flags: 0,
        _rest_of_union: [0; 22],
    };
    request.name[..2].copy_from_slice(b"lo");
    // SAFETY: `request` has the layout of `struct ifreq`, which these
    // requests read and write.
    check(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) })?;
    request.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    // SAFETY: as above.
    check(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &request) })?;
    Ok(())
}

/// A pipe whose ends are closed on exec.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: pipe2 just opened both, and nothing else owns them.
    let fds = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok(fds)
}

/// Read up to `buf.len()` bytes, retrying if interrupted.
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: the pointer and length describe `buf`, valid for writes.
        let result = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if result >= 0 {
            return Ok(result as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Make `fd` the process's standard input.
pub fn set_stdin(fd: RawFd) -> io::Result<()> {
    // SAFETY: dup2 takes no pointers.
    check(unsafe { libc::dup2(fd, libc::STDIN_FILENO) })?;
    Ok(())
}

/// Replace this process with `program`. Only returns on failure, with the
/// error. Async-signal-safe, so it can be used right after fork.
pub fn execve(program: &CStr, args: &[*const libc::c_char], env: &[*const libc::c_char]) -> io::Error {
    // SAFETY: the caller passes NUL-terminated strings, and arrays ending in
    // a null pointer, that outlive the call.
    unsafe { libc::execve(program.as_ptr(), args.as_ptr(), env.as_ptr()) };
    io::Error::last_os_error()
}

/// Write all of `data`, ignoring errors. Async-signal-safe.
pub fn write_ignoring_errors(fd: RawFd, data: &[u8]) {
    // SAFETY: the pointer and length describe `data`.
    let _ = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
}

/// Exit immediately, without running destructors or flushing buffers that
/// belong to the parent. Async-signal-safe.
pub fn exit_immediately(code: libc::c_int) -> ! {
    // SAFETY: _exit takes no pointers and never returns.
    unsafe { libc::_exit(code) }
}
