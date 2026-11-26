//! Socket path resolution, directory ownership checks, peer credential verification, and
//! locating the `g3d-termd` binary (termd protocol 1.5, "Transport").
//!
//! Uses only `std` (`getuid` and `getpeereid`/`getsockopt` come from the system C library
//! that `std` already links on Unix).

use std::io;
use std::path::{Path, PathBuf};

/// Maximum socket path length accepted by `g3d-termd` across macOS (`sun_path` 104 B) and Linux.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Which `g3d-termd` daemon to connect to or start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Named namespace resolved via `$G3D_TERMD_DIR`, `/run/user/<uid>/g3d`, or `/tmp/g3d-<uid>`.
    Namespace(String),
    /// Named namespace inside an explicit `G3D_TERMD_DIR` directory (used by hermetic tests so
    /// parallel test threads never mutate the process-wide environment).
    NamespaceIn { dir: PathBuf, ns: String },
    /// Explicit Unix socket path (`g3d-termd --socket <path>`).
    Socket(PathBuf),
}

impl Target {
    /// Targets namespace `ns` using the standard socket directory rule.
    pub fn namespace(ns: impl Into<String>) -> Self {
        Target::Namespace(ns.into())
    }

    /// Targets namespace `ns` inside an explicit `G3D_TERMD_DIR` directory `dir`.
    pub fn namespace_in(dir: impl Into<PathBuf>, ns: impl Into<String>) -> Self {
        Target::NamespaceIn { dir: dir.into(), ns: ns.into() }
    }

    /// Targets an explicit Unix stream socket path.
    pub fn socket(path: impl Into<PathBuf>) -> Self {
        Target::Socket(path.into())
    }

    /// Validates namespace syntax and resolves the Unix socket path, rejecting paths past
    /// [`MAX_SOCKET_PATH_BYTES`].
    pub fn resolve_socket(&self) -> Result<PathBuf, String> {
        let path = match self {
            Target::Namespace(ns) => {
                validate_name("namespace", ns)?;
                socket_path(ns)
            }
            Target::NamespaceIn { dir, ns } => {
                validate_name("namespace", ns)?;
                dir.join(format!("termd-{ns}.sock"))
            }
            Target::Socket(path) => {
                if path.as_os_str().is_empty() {
                    return Err("termd socket path must not be empty".into());
                }
                path.clone()
            }
        };
        let byte_len = path.as_os_str().len();
        if byte_len > MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "termd socket path {} is {byte_len} bytes, exceeding the {MAX_SOCKET_PATH_BYTES}-byte Unix socket limit",
                path.display()
            ));
        }
        Ok(path)
    }

    /// Resolves where `g3d-termd` writes its log for this target.
    pub fn log_path(&self) -> PathBuf {
        match self {
            Target::Namespace(ns) => log_path(ns),
            Target::NamespaceIn { dir, ns } => dir.join(format!("termd-{ns}.log")),
            Target::Socket(path) => PathBuf::from(format!("{}.log", path.display())),
        }
    }

    pub(crate) fn daemon_args(&self) -> Vec<String> {
        match self {
            Target::Namespace(ns) | Target::NamespaceIn { ns, .. } => {
                vec!["--ns".into(), ns.clone(), "--daemon".into()]
            }
            Target::Socket(path) => {
                vec!["--socket".into(), path.display().to_string(), "--daemon".into()]
            }
        }
    }

    pub(crate) fn explicit_termd_dir(&self) -> Option<&Path> {
        match self {
            Target::Namespace(_) => None,
            Target::NamespaceIn { dir, .. } => Some(dir.as_path()),
            Target::Socket(path) => path.parent().filter(|parent| !parent.as_os_str().is_empty()),
        }
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|val| !val.is_empty()).map(PathBuf::from)
}

/// Validates a namespace or terminal identifier: 1 to 64 ASCII bytes of `[A-Za-z0-9._-]`.
pub fn validate_name(what: &str, name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(format!("{what} {name:?} must be 1 to 64 bytes of [A-Za-z0-9._-]"))
    }
}

/// Current effective/real user ID from the OS.
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

/// Returns `/run/user/<uid>` when it exists and is a real `0700` directory owned by `<uid>`.
#[cfg(unix)]
pub fn user_runtime_dir() -> Option<PathBuf> {
    let uid = current_uid();
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    is_owned_private_dir(&dir, uid).then_some(dir)
}

/// Socket directory rule without `$G3D_TERMD_DIR`: `<runtime>/g3d`, else `/tmp/g3d-<uid>`.
pub fn socket_dir_for(runtime: Option<PathBuf>, uid: u32) -> PathBuf {
    runtime.map(|dir| dir.join("g3d")).unwrap_or_else(|| PathBuf::from(format!("/tmp/g3d-{uid}")))
}

/// Resolves the active `g3d-termd` socket directory (`$G3D_TERMD_DIR`, else [`socket_dir_for`]).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    non_empty_env_path("G3D_TERMD_DIR")
        .unwrap_or_else(|| socket_dir_for(user_runtime_dir(), current_uid()))
}

/// Socket path for namespace `ns`.
#[cfg(unix)]
pub fn socket_path(ns: &str) -> PathBuf {
    socket_dir().join(format!("termd-{ns}.sock"))
}

/// Log file path for namespace `ns`.
#[cfg(unix)]
pub fn log_path(ns: &str) -> PathBuf {
    let dir = non_empty_env_path("G3D_TERMD_DIR")
        .or_else(|| non_empty_env_path("XDG_STATE_HOME").map(|state| state.join("g3d")))
        .or_else(|| non_empty_env_path("HOME").map(|home| home.join(".local/state/g3d")))
        .unwrap_or_else(socket_dir);
    dir.join(format!("termd-{ns}.log"))
}

/// Verifies that `socket_path`'s parent directory is a real (non-symlink) directory owned by
/// `uid` with no group or other permissions (`mode & 0o077 == 0`).
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
            format!("termd socket parent {} is not a real directory", clean.display()),
        ));
    }
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "termd socket directory {} is owned by uid {}, expected uid {uid}",
                clean.display(),
                meta.uid()
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "termd socket directory {} has unsafe mode {:04o} (group/other bits must be 0)",
                clean.display(),
                meta.mode() & 0o7777
            ),
        ));
    }
    Ok(())
}

/// Queries the effective UID of the peer at the other end of a connected `UnixStream`.
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

/// Queries the effective UID of the peer at the other end of a connected `UnixStream`.
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

    let mut cred = Ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    // SAFETY: `sock` holds an open Unix stream socket fd, `cred` is writable storage of `len`
    // bytes, and `len` is verified after `getsockopt` returns 0.
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

/// Ensures the connected peer belongs to `expected_uid`.
#[cfg(unix)]
pub fn verify_peer_uid(sock: &std::os::unix::net::UnixStream, expected_uid: u32) -> io::Result<()> {
    let actual = peer_uid(sock)?;
    if actual != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("termd socket peer is uid {actual}, expected uid {expected_uid}"),
        ));
    }
    Ok(())
}

/// Locates `g3d-termd`: beside the current executable, in `~/.local/bin`, or on `$PATH`.
pub fn find_termd() -> Result<PathBuf, String> {
    let mut looked = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("g3d-termd");
            if candidate.is_file() {
                return Ok(candidate);
            }
            looked.push(candidate.display().to_string());
        }
    }
    if let Some(home) = non_empty_env_path("HOME") {
        let local_bin = home.join(".local/bin/g3d-termd");
        if local_bin.is_file() {
            return Ok(local_bin);
        }
        looked.push(local_bin.display().to_string());
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("g3d-termd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    looked.push("$PATH".into());
    Err(format!("no g3d-termd binary found (looked at {})", looked.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_dir_follows_runtime_else_tmp_uid_rule() {
        assert_eq!(
            socket_dir_for(Some(PathBuf::from("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/g3d")
        );
        assert_eq!(socket_dir_for(None, 501), PathBuf::from("/tmp/g3d-501"));
    }

    #[test]
    fn target_validates_namespace_and_socket_length() {
        assert!(validate_name("namespace", "rs-wall_1.0").is_ok());
        assert!(validate_name("namespace", "").is_err());
        assert!(validate_name("namespace", "bad/slash").is_err());
        assert!(validate_name("namespace", &"a".repeat(65)).is_err());

        let long_path = PathBuf::from(format!("/tmp/{}.sock", "x".repeat(100)));
        assert!(Target::socket(long_path).resolve_socket().is_err());
    }

    #[test]
    fn check_socket_parent_rejects_group_accessible_and_symlink_dirs() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let uid = current_uid();
        let root = PathBuf::from(format!("/tmp/g3d-pchk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&root).unwrap();

        let ok_dir = root.join("ok");
        std::fs::DirBuilder::new().mode(0o700).create(&ok_dir).unwrap();
        assert!(check_socket_parent(&ok_dir.join("termd-x.sock"), uid).is_ok());
        assert!(check_socket_parent(&ok_dir.join("termd-x.sock"), uid.wrapping_add(1)).is_err());

        let sym_dir = root.join("sym");
        std::os::unix::fs::symlink(&ok_dir, &sym_dir).unwrap();
        assert_eq!(
            check_socket_parent(&sym_dir.join("termd-x.sock"), uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );

        let open_dir = root.join("open");
        std::fs::DirBuilder::new().mode(0o700).create(&open_dir).unwrap();
        std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            check_socket_parent(&open_dir.join("termd-x.sock"), uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
