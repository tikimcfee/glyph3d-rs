//! Control socket server owned by the native wall window.

use std::io::{self, BufReader};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::paths::{current_uid, ensure_socket_parent, verify_peer_uid, Target};
use crate::wire::{
    read_request_line, write_reply_frame, Error, InboundLine, MAX_REQUEST_BYTES,
};

/// Callback invoked on a background connection thread each time a new [`Request`] is
/// queued for the wall's main thread (matches `Arc<dyn Fn() + Send + Sync>` in `native`).
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

/// One-shot reply sender paired with an inbound [`Request`].
#[derive(Debug)]
pub struct Responder {
    tx: SyncSender<String>,
}

impl Responder {
    /// Send `reply_text` back to the waiting client. Returns `true` if the client
    /// connection was still waiting for the reply.
    pub fn send(self, reply_text: impl Into<String>) -> bool {
        self.tx.send(reply_text.into()).is_ok()
    }
}

/// One verb line received from a local client, paired with its one-shot reply channel.
#[derive(Debug)]
pub struct Request {
    line: String,
    responder: Responder,
}

impl Request {
    /// Borrow the single-line command sent by the client (without trailing `\r` or `\n`).
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Split this request into its owned command `String` and one-shot [`Responder`].
    pub fn into_parts(self) -> (String, Responder) {
        (self.line, self.responder)
    }

    /// Send `reply_text` back to the waiting client (`"ERR: ..."` for refusals).
    /// Returns `true` if the client was still connected to receive it.
    pub fn reply(self, reply_text: impl Into<String>) -> bool {
        self.responder.send(reply_text)
    }
}

/// Builder configuring a [`Server`] before binding its Unix domain socket.
pub struct ServerBuilder {
    target: Target,
    wake: Option<WakeFn>,
    expected_uid: u32,
}

impl ServerBuilder {
    /// Start building a server for `target`, requiring peers to match [`current_uid`].
    pub fn new(target: Target) -> Self {
        ServerBuilder {
            target,
            wake: None,
            expected_uid: current_uid(),
        }
    }

    /// Set the wake callback called after each [`Request`] is queued onto [`Server::requests`].
    pub fn wake(mut self, wake: WakeFn) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Override the peer UID required on incoming connections (defaults to [`current_uid`];
    /// exposed so tests can exercise live foreign-UID refusal without root).
    pub fn expected_uid(mut self, uid: u32) -> Self {
        self.expected_uid = uid;
        self
    }

    /// Bind the control socket (taking over a dead stale socket if present, or refusing
    /// if a live wall already listens there) and start the accept thread.
    pub fn bind(self) -> Result<Server, Error> {
        let socket_path = self
            .target
            .resolve_socket()
            .map_err(Error::InvalidTarget)?;
        let owner_uid = current_uid();
        ensure_socket_parent(&socket_path, owner_uid)?;

        let listener = bind_or_replace_stale(&socket_path, self.target.namespace_name())?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        let bound_ino = std::fs::symlink_metadata(&socket_path)?.ino();

        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_accept = Arc::clone(&shutdown);
        let wake = self.wake.unwrap_or_else(|| Arc::new(|| {}));
        let expected_uid = self.expected_uid;

        let accept_thread = thread::Builder::new()
            .name("wall-control-accept".into())
            .spawn(move || accept_loop(listener, req_tx, wake, expected_uid, shutdown_accept))
            .map_err(Error::Io)?;

        Ok(Server {
            socket_path,
            bound_ino,
            req_rx,
            shutdown,
            accept_thread: Some(accept_thread),
        })
    }
}

fn bind_or_replace_stale(
    socket_path: &Path,
    namespace: Option<&str>,
) -> Result<UnixListener, Error> {
    match UnixListener::bind(socket_path) {
        Ok(listener) => return Ok(listener),
        Err(err) if err.kind() != io::ErrorKind::AddrInUse => return Err(Error::Io(err)),
        Err(_) => {}
    }

    let meta = match std::fs::symlink_metadata(socket_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return UnixListener::bind(socket_path).map_err(Error::Io);
        }
        Err(err) => return Err(Error::Io(err)),
    };
    if !meta.file_type().is_socket() {
        return Err(Error::NotASocket(socket_path.to_path_buf()));
    }

    match UnixStream::connect(socket_path) {
        Ok(live_probe) => {
            drop(live_probe);
            Err(Error::AlreadyRunning {
                path: socket_path.to_path_buf(),
                namespace: namespace.map(str::to_string),
            })
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            match std::fs::remove_file(socket_path) {
                Ok(()) => {}
                Err(rm_err) if rm_err.kind() == io::ErrorKind::NotFound => {}
                Err(rm_err) => return Err(Error::Io(rm_err)),
            }
            UnixListener::bind(socket_path).map_err(Error::Io)
        }
        Err(err) => Err(Error::Io(err)),
    }
}

fn accept_loop(
    listener: UnixListener,
    req_tx: Sender<Request>,
    wake: WakeFn,
    expected_uid: u32,
    shutdown: Arc<AtomicBool>,
) {
    let mut clients: Vec<JoinHandle<()>> = Vec::new();
    for incoming in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
        };
        if verify_peer_uid(&stream, expected_uid).is_err() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            continue;
        }

        // Reap any finished client threads so long-lived sessions never accumulate handles.
        clients.retain(|h| !h.is_finished());

        let tx = req_tx.clone();
        let wake_clone = Arc::clone(&wake);
        let shutdown_clone = Arc::clone(&shutdown);
        if let Ok(handle) = thread::Builder::new()
            .name("wall-control-client".into())
            .spawn(move || handle_client(stream, tx, wake_clone, shutdown_clone))
        {
            clients.push(handle);
        }
    }
}

fn handle_client(
    stream: UnixStream,
    req_tx: Sender<Request>,
    wake: WakeFn,
    shutdown: Arc<AtomicBool>,
) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match read_request_line(&mut reader) {
            Ok(InboundLine::Eof) | Err(_) => break,
            Ok(InboundLine::TooLong) => {
                let msg = format!("ERR: command line exceeds {MAX_REQUEST_BYTES} bytes");
                let _ = write_reply_frame(&mut writer, &msg);
                break;
            }
            Ok(InboundLine::InvalidUtf8) => {
                let _ = write_reply_frame(&mut writer, "ERR: command line is not valid UTF-8");
                break;
            }
            Ok(InboundLine::Line(line)) => {
                let (reply_tx, reply_rx) = mpsc::sync_channel::<String>(1);
                let req = Request {
                    line,
                    responder: Responder { tx: reply_tx },
                };
                if req_tx.send(req).is_err() {
                    break;
                }
                wake();
                let Ok(reply_text) = reply_rx.recv() else {
                    break;
                };
                if write_reply_frame(&mut writer, &reply_text).is_err() {
                    break;
                }
            }
        }
    }
}

/// Local Unix-socket control server owned by the wall application.
///
/// Dropping [`Server`] signals its accept loop to stop, wakes `accept()`, joins the
/// accept thread, and unlinks its socket file (verified by inode).
pub struct Server {
    socket_path: PathBuf,
    bound_ino: u64,
    req_rx: Receiver<Request>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Bind the control socket for `target` and call `wake` whenever a new [`Request`]
    /// arrives on [`Self::requests`].
    pub fn bind(target: &Target, wake: WakeFn) -> Result<Self, Error> {
        ServerBuilder::new(target.clone()).wake(wake).bind()
    }

    /// Bind the control socket for `target` without a wake callback.
    pub fn bind_without_wake(target: &Target) -> Result<Self, Error> {
        ServerBuilder::new(target.clone()).bind()
    }

    /// Construct a [`ServerBuilder`] for `target`.
    pub fn builder(target: Target) -> ServerBuilder {
        ServerBuilder::new(target)
    }

    /// Resolved filesystem path of the bound Unix domain socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Channel receiver yielding inbound [`Request`] items in arrival order.
    pub fn requests(&self) -> &Receiver<Request> {
        &self.req_rx
    }

    /// Non-blocking poll returning the next queued [`Request`], if any.
    pub fn try_recv(&self) -> Option<Request> {
        self.req_rx.try_recv().ok()
    }

    /// Block until the next [`Request`] arrives or all senders close.
    pub fn recv(&self) -> Result<Request, RecvError> {
        self.req_rx.recv()
    }

    /// Block up to `timeout` waiting for the next [`Request`].
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Request, RecvTimeoutError> {
        self.req_rx.recv_timeout(timeout)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the accept thread's blocking `listener.accept()` call immediately.
        // If the socket path was removed externally or connect is blocked, detach
        // rather than hanging teardown.
        if UnixStream::connect(&self.socket_path).is_ok() {
            if let Some(handle) = self.accept_thread.take() {
                let _ = handle.join();
            }
        }
        if let Ok(meta) = std::fs::symlink_metadata(&self.socket_path) {
            if meta.ino() == self.bound_ino {
                let _ = std::fs::remove_file(&self.socket_path);
            }
        }
    }
}
