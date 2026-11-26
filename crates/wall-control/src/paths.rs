//! Socket path resolution, directory ownership checks, and peer UID credential
//! verification for the wall's local control socket.
//!
//! Matches `crates/termd-client/src/paths.rs`'s directory and peer-check rules:
//! - Directory: `$G3D_TERMD_DIR`, else `/run/user/<uid>/g3d` (when `/run/user/<uid>`
//!   is a real `0700` directory owned by `<uid>`), else `/tmp/g3d-<uid>`.
//! - File name: `wall-<namespace>.sock` (default namespace: [`DEFAULT_NAMESPACE`], `"rs-wall"`).
//! - Permissions: parent directory `0700`, socket inode `0600`, and connected peer
//!   effective UID verified equal to `current_uid()` via `getpeereid` (macOS/BSD) or
//!   `SO_PEERCRED` (Linux).

use std::io;
use std::path::{Path, PathBuf};

/// Default namespace of the native wall (`"rs-wall"`).
pub const DEFAULT_NAMESPACE: &str = "rs-wall";

/// Maximum Unix socket path length in bytes across macOS (`sun_path` 104 B) and Linux.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Which wall control socket to bind or connect to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Named namespace resolved via `$G3D_TERMD_DIR`, `/run/user/<uid>/g3d`, or `/tmp/g3d-<uid>`.
    Namespace(String),
    /// Named namespace inside an explicit directory (used by hermetic tests so parallel
    /// test threads never mutate process-wide environment variables).
    NamespaceIn { dir: PathBuf, ns: String },
    /// Explicit Unix domain stream socket path.
    Socket(PathBuf),
}

impl Default for Target {
    fn default() -> Self {
        Target::Namespace(DEFAULT_NAMESPACE.to_string())
    }
}

impl Target {
    /// Target namespace `ns` using the standard `g3d` socket directory rule.
    pub fn namespace(ns: impl Into<String>) -> Self {
        Target::Namespace(ns.into())
    }

    /// Target namespace `ns` inside an explicit directory `dir` (`<dir>/wall-<ns>.sock`).
    pub fn namespace_in(dir: impl Into<PathBuf>, ns: impl Into<String>) -> Self {
        Target::NamespaceIn {
            dir: dir.into(),
            ns: ns.into(),
        }
    }

    /// Target an explicit Unix domain stream socket path.
    pub fn socket(path: impl Into<PathBuf>) -> Self {
        Target::Socket(path.into())
    }

    /// Namespace name when targeting a namespace, or `None` for an explicit socket path.
    pub fn namespace_name(&self) -> Option<&str> {
        match self {
            Target::Namespace(ns) | Target::NamespaceIn { ns, .. } => Some(ns.as_str()),
            Target::Socket(_) => None,
        }
    }

    /// Validate namespace syntax and resolve the Unix socket path, rejecting paths
    /// longer than [`MAX_SOCKET_PATH_BYTES`].
    pub fn resolve_socket(&self) -> Result<PathBuf, String> {
        let path = match self {
            Target::Namespace(ns) => {
                validate_namespace(ns)?;
                socket_path(ns)
            }
            Target::NamespaceIn { dir, ns } => {
                validate_namespace(ns)?;
                dir.join(format!("wall-{ns}.sock"))
            }
            Target::Socket(path) => {
                if path.as_os_str().is_empty() {
                    return Err("wall socket path must not be empty".into());
                }
                path.clone()
            }
        };
        let byte_len = path.as_os_str().len();
        if byte_len > MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "wall socket path {} is {byte_len} bytes, exceeding the {MAX_SOCKET_PATH_BYTES}-byte Unix socket limit",
                path.display()
            ));
        }
        Ok(path)
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|val| !val.is_empty())
        .map(PathBuf::from)
}

/// Validate a wall namespace identifier: 1 to 64 ASCII bytes of `[A-Za-z0-9._-]`.
pub fn validate_namespace(ns: &str) -> Result<(), String> {
    let valid = !ns.is_empty()
        && ns.len() <= 64
        && ns
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "namespace {ns:?} must be 1 to 64 bytes of [A-Za-z0-9._-]"
        ))
    }
}

/// Current real user ID from the OS.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: POSIX `getuid` takes no arguments, reads kernel process credentials,
    // is thread-safe, and cannot fail.
    unsafe { getuid() }
}

#[cfg(unix)]
fn is_owned_private_dir(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    meta.file_type().is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o077 == 0
}

/// Return `/run/user/<uid>` when it exists and is a real `0700` directory owned by `<uid>`.
#[cfg(unix)]
pub fn user_runtime_dir() -> Option<PathBuf> {
    let uid = current_uid();
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    is_owned_private_dir(&dir, uid).then_some(dir)
}

/// Socket directory rule without `$G3D_TERMD_DIR`: `<runtime>/g3d`, else `/tmp/g3d-<uid>`.
pub fn socket_dir_for(runtime: Option<PathBuf>, uid: u32) -> PathBuf {
    runtime
        .map(|dir| dir.join("g3d"))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/g3d-{uid}")))
}

/// Resolve the active `g3d` socket directory (`$G3D_TERMD_DIR`, else [`socket_dir_for`]).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    non_empty_env_path("G3D_TERMD_DIR")
        .unwrap_or_else(|| socket_dir_for(user_runtime_dir(), current_uid()))
}

/// Control socket path (`<socket_dir>/wall-<ns>.sock`) for namespace `ns`.
#[cfg(unix)]
pub fn socket_path(ns: &str) -> PathBuf {
    socket_dir().join(format!("wall-{ns}.sock"))
}

/// Create `socket_path`'s parent directory with mode `0700` if it does not yet exist,
/// then verify ownership and `0700` privacy via [`check_socket_parent`].
#[cfg(unix)]
pub fn ensure_socket_parent(socket_path: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(parent) = socket_path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        let clean: PathBuf = parent.components().collect();
        if std::fs::symlink_metadata(&clean).is_err() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&clean)?;
            std::fs::set_permissions(&clean, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    check_socket_parent(socket_path, uid)
}

/// Verify that `socket_path`'s parent directory is a real (non-symlink) directory
/// owned by `uid` with no group or other permissions (`mode & 0o077 == 0`).
#[cfg(unix)]
pub fn check_socket_parent(socket_path: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = socket_path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    let clean: PathBuf = parent.components().collect();
    let meta = std::fs::symlink_metadata(&clean)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "wall socket parent {} is not a real directory",
                clean.display()
            ),
        ));
    }
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "wall socket directory {} is owned by uid {}, expected uid {uid}",
                clean.display(),
                meta.uid()
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "wall socket directory {} has unsafe mode {:04o} (group/other bits must be 0)",
                clean.display(),
                meta.mode() & 0o7777
            ),
        ));
    }
    Ok(())
}

/// Query the effective UID of the peer at the other end of a connected `UnixStream`.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
pub fn peer_uid(sock: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn getpeereid(
            socket: std::os::raw::c_int,
            euid: *mut u32,
            egid: *mut u32,
        ) -> std::os::raw::c_int;
    }
    let mut uid: u32 = 0;
    let mut gid: u32 = 0;
    // SAFETY: `sock` holds an open file descriptor for the duration of this call, and
    // `&mut uid` and `&mut gid` are valid aligned out-pointers.
    let rc = unsafe { getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// Query the effective UID of the peer at the other end of a connected `UnixStream`.
#[cfg(target_os = "linux")]
pub fn peer_uid(sock: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_int, c_void};

    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }

    #[cfg(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))]
    const SOL_SOCKET: c_int = 0xffff;
    #[cfg(not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )))]
    const SOL_SOCKET: c_int = 1;

    #[cfg(any(target_arch = "sparc", target_arch = "sparc64"))]
    const SO_PEERCRED: c_int = 0x0040;
    #[cfg(not(any(target_arch = "sparc", target_arch = "sparc64")))]
    const SO_PEERCRED: c_int = 17;

    extern "C" {
        fn getsockopt(
            sockfd: c_int,
            level: c_int,
            optname: c_int,
            optval: *mut c_void,
            optlen: *mut u32,
        ) -> c_int;
    }

    let mut cred = Ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    // SAFETY: `sock` holds an open Unix stream socket fd, `cred` is writable storage of
    // `len` bytes, and `len` is verified after `getsockopt` returns 0.
    let rc = unsafe {
        getsockopt(
            sock.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut cred as *mut Ucred).cast::<c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<Ucred>() {
        return Err(io::Error::other("unexpected SO_PEERCRED length"));
    }
    Ok(cred.uid)
}

/// Ensure the connected peer belongs to `expected_uid`.
#[cfg(unix)]
pub fn verify_peer_uid(sock: &std::os::unix::net::UnixStream, expected_uid: u32) -> io::Result<()> {
    let actual = peer_uid(sock)?;
    if actual != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("wall socket peer is uid {actual}, expected uid {expected_uid}"),
        ));
    }
    Ok(())
}
