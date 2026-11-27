//! Namespace connection hub: owns `termd-client`, routes per-terminal frames onto dedicated
//! session threads, tracks daemon restarts/disconnects, and spawns/adopts/kills terminals.

use crate::coalesce::CoalescePolicy;
use crate::inbox::SessionInbox;
use crate::session::{run_async_cmd_worker, AsyncCmd, AsyncCmdQueue, TerminalSession};
use glyph_vt::Theme;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use termd_client::{
    Colors, ConnectOptions, Create, Error, Event, Flow, IfExists, Mode, Role, Subscribe,
    Subscription, Target, TermInfo, Termd, TermdHandle, Welcome,
};

/// Current lifecycle state of the underlying `g3d-termd` connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    /// Connected to a live brain (`restarted` becomes `true` once at least one restart was ridden).
    Connected { generation: Option<u64>, restarted: bool },
    /// Daemon connection ended permanently (namespace stopped, instance replaced, or timed out).
    Gone { restart: bool, message: String },
}

/// Connection-level events reported to the host via [`SessionHub::poll_daemon_events`].
#[derive(Clone, Debug, PartialEq)]
pub enum DaemonEvent {
    /// Client rode a daemon brain restart (`ERROR restart` when `restart` is true, or crash),
    /// re-subscribed active terminals, and requested repaints.
    Reconnected { restart: bool, generation: Option<u64> },
    /// Daemon connection ended permanently without recovery.
    Gone { restart: bool, message: String },
    /// Pushed status document when `status_follow` is active.
    Status(serde_json::Value),
    /// Requested daemon upgrade failed; previous brain continues serving.
    UpgradeFailed { message: String },
}

/// Configuration for [`SessionHub::open_with`].
#[derive(Clone, Debug)]
pub struct HubOptions {
    pub client_name: String,
    pub connect: ConnectOptions,
    pub theme: Theme,
    pub coalesce: CoalescePolicy,
}

impl Default for HubOptions {
    fn default() -> Self {
        Self {
            client_name: "glyph3d-wall".into(),
            connect: ConnectOptions::default(),
            theme: Theme::default(),
            coalesce: CoalescePolicy::default(),
        }
    }
}

/// Parameters for [`SessionHub::spawn`].
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    pub id: String,
    pub argv: Option<Vec<String>>,
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, Option<String>>>,
    pub env_base: Option<BTreeMap<String, String>>,
    pub if_exists: Option<IfExists>,
    pub scrollback_bytes: Option<u64>,
    pub colors: Option<Colors>,
    pub cell_px: Option<[u32; 2]>,
}

impl SpawnOptions {
    /// Default login-shell spawn options for terminal `id` at `(cols, rows)`.
    pub fn shell(id: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            id: id.into(),
            argv: None,
            cols,
            rows,
            cwd: None,
            env: None,
            env_base: None,
            if_exists: None,
            scrollback_bytes: None,
            colors: None,
            cell_px: None,
        }
    }
}

struct RoutingTable {
    inboxes: HashMap<u32, Arc<SessionInbox>>,
    /// Frames that arrived between `subscribe`'s `REPLY` and `register_session` on the caller
    /// thread, held briefly so an initial `REPAINT`/`Modes`/`Title` can never be lost on multicore.
    early: HashMap<u32, Vec<Event>>,
    closed_handles: VecDeque<u32>,
    /// In-flight `subscribe`/`create_and_subscribe` RPCs whose handles have not yet been
    /// registered in `inboxes`. While `> 0`, frames for handles in `closed_handles` are stashed
    /// in `early` rather than dropped, because `g3d-termd` can reuse a recently unsubscribed
    /// handle and stream its initial `REPAINT` before `register_session` runs.
    pending_subscribes: usize,
}

impl RoutingTable {
    fn prune_closed_early(&mut self) {
        if self.pending_subscribes == 0 {
            let closed = &self.closed_handles;
            self.early.retain(|h, _| !closed.contains(h));
        }
    }
}

struct PendingSubscribeGuard<'a> {
    inner: &'a HubInner,
    armed: bool,
}

impl PendingSubscribeGuard<'_> {
    fn finish(mut self, handle: u32, inbox: Arc<SessionInbox>) {
        self.armed = false;
        let mut table = self.inner.routing.lock().expect("routing mutex poisoned");
        table.closed_handles.retain(|&h| h != handle);
        if let Some(stashed) = table.early.remove(&handle) {
            for ev in stashed {
                let _ = inbox.push(ev);
            }
        }
        table.inboxes.insert(handle, inbox);
        table.pending_subscribes = table.pending_subscribes.saturating_sub(1);
        table.prune_closed_early();
    }
}

impl Drop for PendingSubscribeGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut table = self.inner.routing.lock().expect("routing mutex poisoned");
        table.pending_subscribes = table.pending_subscribes.saturating_sub(1);
        table.prune_closed_early();
    }
}

/// Structural fields of `g3d-termd`'s pushed `Event::Status` document whose change should wake
/// the window even while the Status panel is closed. Volatile telemetry (`at`, `out_bps`,
/// `daemon.brain.rss`, `daemon.snapshots`, `saved_seq`) is excluded so an idle wall is not woken
/// once per second while `out_bps` decays or RSS fluctuates.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusWakeSig {
    daemon_exiting: bool,
    daemon_version: Option<String>,
    daemon_instance: Option<String>,
    errors: Vec<String>,
    terminals: Vec<TerminalStatusSig>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TerminalStatusSig {
    id: String,
    exited: bool,
    scrollback_rows: u64,
    title: String,
    pwd: String,
    cwd: String,
    alt: bool,
}

fn status_wake_sig(doc: &serde_json::Value) -> StatusWakeSig {
    let daemon = doc.get("daemon");
    let daemon_exiting = daemon
        .and_then(|d| d.get("exiting"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let daemon_version = daemon
        .and_then(|d| d.get("version"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let daemon_instance = daemon
        .and_then(|d| d.get("instance"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let errors = doc
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .map(|arr| arr.iter().map(serde_json::Value::to_string).collect())
        .unwrap_or_default();
    let mut terminals: Vec<TerminalStatusSig> = doc
        .get("terminals")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|t| TerminalStatusSig {
                    id: t
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    exited: t.get("exit").is_some_and(|e| !e.is_null()),
                    scrollback_rows: t
                        .get("scrollback_rows")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    title: t
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    pwd: t
                        .get("pwd")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    cwd: t
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    alt: t
                        .get("alt")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    terminals.sort();
    StatusWakeSig {
        daemon_exiting,
        daemon_version,
        daemon_instance,
        errors,
        terminals,
    }
}

struct DaemonTracking {
    state: DaemonState,
    events: VecDeque<DaemonEvent>,
    last_status_sig: Option<StatusWakeSig>,
}

pub(crate) struct HubInner {
    termd_handle: TermdHandle,
    termd_owner: Mutex<Option<Termd>>,
    routing: Mutex<RoutingTable>,
    daemon: Mutex<DaemonTracking>,
    async_queue: Arc<AsyncCmdQueue>,
    async_worker: Mutex<Option<JoinHandle<()>>>,
    router_thread: Mutex<Option<JoinHandle<()>>>,
    closing: AtomicBool,
    status_wake_all: AtomicBool,
    theme: Mutex<Theme>,
    options: HubOptions,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Drop for HubInner {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.async_queue.close();
        {
            let table = self.routing.lock().expect("routing mutex poisoned");
            for inbox in table.inboxes.values() {
                inbox.close();
            }
        }
        // Dropping `Termd` shuts down the socket and joins `termd-client`'s reader thread, which
        // closes `events()` and causes `router_thread` to exit immediately.
        if let Ok(mut owner) = self.termd_owner.lock() {
            drop(owner.take());
        }
        if let Ok(mut router) = self.router_thread.lock() {
            if let Some(join) = router.take() {
                let _ = join.join();
            }
        }
        if let Ok(mut worker) = self.async_worker.lock() {
            if let Some(join) = worker.take() {
                let _ = join.join();
            }
        }
    }
}

impl HubInner {
    pub(crate) fn termd_handle(&self) -> TermdHandle {
        self.termd_handle.clone()
    }

    pub(crate) fn wake_fn(&self) -> Arc<dyn Fn() + Send + Sync> {
        Arc::clone(&self.wake)
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn current_theme(&self) -> Theme {
        self.theme.lock().expect("theme mutex poisoned").clone()
    }

    pub(crate) fn set_theme(&self, theme: Theme) {
        *self.theme.lock().expect("theme mutex poisoned") = theme;
    }

    pub(crate) fn send_async(&self, cmd: AsyncCmd) {
        self.async_queue.push(cmd);
    }

    fn begin_subscribe(&self) -> PendingSubscribeGuard<'_> {
        let mut table = self.routing.lock().expect("routing mutex poisoned");
        table.pending_subscribes += 1;
        PendingSubscribeGuard {
            inner: self,
            armed: true,
        }
    }

    pub(crate) fn unregister_session(&self, handle: u32) {
        let mut table = self.routing.lock().expect("routing mutex poisoned");
        table.inboxes.remove(&handle);
        table.early.remove(&handle);
        if table.closed_handles.len() >= 128 {
            table.closed_handles.pop_front();
        }
        table.closed_handles.push_back(handle);
    }
}

/// Shared connection to one `g3d-termd` namespace (or socket) that spawns, adopts, and routes
/// frames to per-terminal [`TerminalSession`] worker threads.
///
/// Cloning `SessionHub` shares the same underlying connection (`Arc`), and every
/// [`TerminalSession`] keeps the hub alive until the last session is dropped.
#[derive(Clone)]
pub struct SessionHub {
    inner: Arc<HubInner>,
}

impl SessionHub {
    /// Connects to `target` (starting `g3d-termd --daemon` if nothing listens) with default
    /// [`HubOptions`]. `wake` is called whenever a session publishes a coalesced screen update or
    /// a daemon lifecycle event arrives.
    pub fn open(target: &Target, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, Error> {
        Self::open_with(target, HubOptions::default(), wake)
    }

    /// Connects to `target` (starting `g3d-termd --daemon` if nothing listens) with custom
    /// [`HubOptions`].
    pub fn open_with(
        target: &Target,
        options: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, Error> {
        // `termd-client`'s raw per-frame wake is intentionally a no-op here: our router thread
        // blocks directly on `termd.events().recv()` and only coalesced session publishes or
        // daemon-level transitions invoke the host's `wake`, preventing RunLoop wakeup floods.
        let termd = Termd::connect_or_start_with(
            target,
            &options.client_name,
            options.connect.clone(),
            Box::new(|| {}),
        )?;
        Self::from_termd(termd, options, wake)
    }

    /// Connects to an already-running daemon without starting one.
    pub fn connect_with(
        target: &Target,
        options: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, Error> {
        let termd = Termd::connect_with(
            target,
            &options.client_name,
            options.connect.clone(),
            Box::new(|| {}),
        )?;
        Self::from_termd(termd, options, wake)
    }

    fn from_termd(
        termd: Termd,
        options: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, Error> {
        let termd_handle = termd.handle();
        let welcome = termd_handle.welcome();
        let async_queue = Arc::new(AsyncCmdQueue::new());
        let worker_queue = Arc::clone(&async_queue);
        let cmd_termd = termd_handle.clone();
        let async_worker = thread::Builder::new()
            .name("term-session-io".into())
            .spawn(move || run_async_cmd_worker(cmd_termd, worker_queue))
            .map_err(Error::Io)?;

        let inner = Arc::new(HubInner {
            termd_handle,
            termd_owner: Mutex::new(Some(termd)),
            routing: Mutex::new(RoutingTable {
                inboxes: HashMap::new(),
                early: HashMap::new(),
                closed_handles: VecDeque::with_capacity(128),
                pending_subscribes: 0,
            }),
            daemon: Mutex::new(DaemonTracking {
                state: DaemonState::Connected {
                    generation: welcome.generation,
                    restarted: false,
                },
                events: VecDeque::new(),
                last_status_sig: None,
            }),
            async_queue,
            async_worker: Mutex::new(Some(async_worker)),
            router_thread: Mutex::new(None),
            closing: AtomicBool::new(false),
            status_wake_all: AtomicBool::new(false),
            theme: Mutex::new(options.theme.clone()),
            options,
            wake,
        });

        let weak_inner = Arc::downgrade(&inner);
        let router_inner = Arc::clone(&inner);
        let router = thread::Builder::new()
            .name("term-session-router".into())
            .spawn(move || router_loop(router_inner, weak_inner))
            .map_err(Error::Io)?;
        *inner.router_thread.lock().expect("router_thread mutex poisoned") = Some(router);

        Ok(Self { inner })
    }

    /// Underlying `termd-client` request handle (e.g. for `upgrade` or custom RPCs).
    pub fn client(&self) -> TermdHandle {
        self.inner.termd_handle()
    }

    /// Current default [`Theme`] used for newly spawned or adopted terminals.
    pub fn theme(&self) -> Theme {
        self.inner.current_theme()
    }

    /// Updates the default [`Theme`] applied to newly spawned or adopted terminals.
    pub fn set_theme(&self, theme: Theme) {
        self.inner.set_theme(theme);
    }

    /// Latest `WELCOME` handshake from `g3d-termd`.
    pub fn welcome(&self) -> Welcome {
        self.inner.termd_handle().welcome()
    }

    /// Current daemon connection state (`Connected` or `Gone`).
    pub fn daemon_state(&self) -> DaemonState {
        self.inner.daemon.lock().expect("daemon mutex poisoned").state.clone()
    }

    /// Drains daemon-level events (`Reconnected`, `Gone`, `Status`, `UpgradeFailed`) in order.
    pub fn poll_daemon_events(&self) -> Vec<DaemonEvent> {
        let mut guard = self.inner.daemon.lock().expect("daemon mutex poisoned");
        guard.events.drain(..).collect()
    }

    /// Lists all live terminals in the connected namespace.
    pub fn list(&self) -> Result<Vec<TermInfo>, Error> {
        self.inner.termd_handle().list()
    }

    /// Subscribes to existing terminal `id` as [`Role::Own`] (optionally resizing to `size`) and
    /// starts its dedicated session thread.
    pub fn attach_owner(
        &self,
        id: &str,
        size: Option<(u16, u16)>,
    ) -> Result<TerminalSession, Error> {
        self.attach_with_role(id, Role::Own, size, 0)
    }

    /// Subscribes to existing terminal `id` with `role`, optional owner `size`, and `history_rows`.
    pub fn attach_with_role(
        &self,
        id: &str,
        role: Role,
        size: Option<(u16, u16)>,
        history_rows: u32,
    ) -> Result<TerminalSession, Error> {
        self.attach_with_options(&Subscribe {
            id: id.to_owned(),
            mode: Mode::Raw,
            role,
            history_rows: (history_rows > 0).then_some(history_rows),
            flow: None,
            cols: size.map(|pair| pair.0),
            rows: size.map(|pair| pair.1),
        })
    }

    /// Subscribes to existing terminal `opts.id` with explicit [`Subscribe`] options (such as
    /// [`Role::Own`] paired with [`Flow::Lossy`] for cross-namespace agent views).
    pub fn attach_with_options(&self, opts: &Subscribe) -> Result<TerminalSession, Error> {
        let pending = self.inner.begin_subscribe();
        let sub = self.inner.termd_handle().subscribe_with(opts)?;
        self.start_subscribed_session(&opts.id, sub, pending)
    }

    fn start_subscribed_session(
        &self,
        id: &str,
        sub: Subscription,
        pending: PendingSubscribeGuard<'_>,
    ) -> Result<TerminalSession, Error> {
        let inbox = Arc::new(SessionInbox::new(self.inner.options.coalesce.inbox_max_bytes));
        pending.finish(sub.handle, Arc::clone(&inbox));
        match TerminalSession::spawn_worker(
            Arc::clone(&self.inner),
            id.to_owned(),
            sub.handle,
            sub.role,
            sub.cols,
            sub.rows,
            self.inner.current_theme(),
            self.inner.options.coalesce,
            inbox,
            self.inner.wake_fn(),
        ) {
            Ok(session) => Ok(session),
            Err(err) => {
                self.inner.unregister_session(sub.handle);
                let _ = self.inner.termd_handle().unsubscribe(sub.handle);
                Err(err)
            }
        }
    }

    /// Creates (or adopts) a terminal in `g3d-termd`, pipelining `create` and `subscribe` in one
    /// socket write so startup `BEL`/mode/title events are caught without a client RTT gap, and
    /// returns its running [`TerminalSession`].
    pub fn spawn(&self, opts: &SpawnOptions) -> Result<TerminalSession, Error> {
        let create_req = Create {
            id: opts.id.clone(),
            argv: opts.argv.clone(),
            cols: opts.cols,
            rows: opts.rows,
            cwd: opts.cwd.clone(),
            env: opts.env.clone(),
            env_base: opts.env_base.clone(),
            if_exists: opts.if_exists,
            scrollback_bytes: opts.scrollback_bytes,
            colors: opts.colors.clone(),
            cell_px: opts.cell_px,
        };
        let sub_req = Subscribe {
            id: opts.id.clone(),
            mode: Mode::Raw,
            role: Role::Own,
            history_rows: None,
            flow: None,
            cols: None,
            rows: None,
        };
        let pending = self.inner.begin_subscribe();
        let (created, sub) = self
            .inner
            .termd_handle()
            .create_and_subscribe(&create_req, &sub_req)?;
        let _ = Flow::Lossy;
        self.start_subscribed_session(&created.id, sub, pending)
    }

    /// Lists all live terminals in the namespace (sorted by `id`) and attaches to each as its
    /// size owner; if the namespace holds none, spawns shell `"t1"` at `(default_cols, default_rows)`.
    pub fn adopt_or_spawn_initial(
        &self,
        default_cols: u16,
        default_rows: u16,
    ) -> Result<Vec<TerminalSession>, Error> {
        let mut ids: Vec<String> =
            self.list()?.into_iter().map(|info| info.id).collect();
        if ids.is_empty() {
            return Ok(vec![self.spawn(&SpawnOptions::shell("t1", default_cols, default_rows))?]);
        }
        ids.sort();
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            sessions.push(self.attach_owner(&id, None)?);
        }
        Ok(sessions)
    }

    /// Kills terminal `id` in `g3d-termd` synchronously.
    pub fn kill(&self, id: &str) -> Result<(), Error> {
        self.inner.termd_handle().kill(id)
    }

    /// Queues a `kill` request for terminal `id` on the background IO thread so a slow process
    /// shutdown (`kill_grace`) never stalls the window thread.
    pub fn send_kill(&self, id: &str) {
        self.inner.send_async(AsyncCmd::Kill { id: id.to_owned() });
    }

    /// Fetches the daemon status document once.
    pub fn status(&self) -> Result<serde_json::Value, Error> {
        self.inner.termd_handle().status()
    }

    /// Controls whether every pushed `Event::Status` wakes the host (e.g. while a live Status
    /// panel is open showing `out_bps` and RSS), or only structural status changes (terminal
    /// list, exit, scrollback, title/pwd/cwd, errors, exiting).
    pub fn set_status_wake_all(&self, wake_all: bool) {
        self.inner.status_wake_all.store(wake_all, Ordering::Release);
    }

    /// Enables pushed [`DaemonEvent::Status`] updates at `interval_ms`.
    pub fn status_follow(&self, interval_ms: Option<u64>) -> Result<u64, Error> {
        self.inner.termd_handle().status_follow(interval_ms)
    }

    /// Stops pushed [`DaemonEvent::Status`] updates.
    pub fn status_unfollow(&self) -> Result<(), Error> {
        self.inner.termd_handle().status_unfollow()
    }

    /// Kills every terminal in the namespace and stops the daemon.
    pub fn stop(&self) -> Result<(), Error> {
        self.inner.termd_handle().stop()
    }
}

fn router_loop(boot_inner: Arc<HubInner>, weak: std::sync::Weak<HubInner>) {
    // Take `Termd` out of `boot_inner` onto the router stack, then drop our strong `Arc<HubInner>`
    // BEFORE entering the loop! Why? If `router_loop` held a strong `Arc<HubInner>`, `HubInner`'s
    // refcount would never reach 0 when the user drops `SessionHub` and all `TerminalSession`s!
    let Some(termd) = boot_inner.termd_owner.lock().expect("termd_owner poisoned").take() else {
        return;
    };
    drop(boot_inner);

    loop {
        // Poll with a short timeout so dropping the last external `SessionHub` / `TerminalSession`
        // (`weak.upgrade() == None`) terminates `router_loop` and drops `termd` cleanly even when
        // the daemon is completely idle!
        match termd.events().recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(ev) => {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if inner.is_closing() {
                    break;
                }
                dispatch_wire_event(&inner, ev);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if weak.strong_count() == 0 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(inner) = weak.upgrade() {
                    if !inner.is_closing() {
                        let notify = {
                            let mut guard = inner.daemon.lock().expect("daemon mutex poisoned");
                            if matches!(guard.state, DaemonState::Gone { .. }) {
                                false
                            } else {
                                let msg = "daemon stopped".to_string();
                                guard.state = DaemonState::Gone {
                                    restart: false,
                                    message: msg.clone(),
                                };
                                guard.events.push_back(DaemonEvent::Gone {
                                    restart: false,
                                    message: msg,
                                });
                                true
                            }
                        };
                        if notify {
                            let table = inner.routing.lock().expect("routing mutex poisoned");
                            for inbox in table.inboxes.values() {
                                inbox.close();
                            }
                            drop(table);
                            (inner.wake)();
                        }
                    }
                }
                break;
            }
        }
    }
    drop(termd);
}

fn dispatch_wire_event(inner: &HubInner, ev: Event) {
    let handle = match &ev {
        Event::Output { handle, .. }
        | Event::Repaint { handle, .. }
        | Event::Size { handle, .. }
        | Event::Title { handle, .. }
        | Event::Modes { handle, .. }
        | Event::Bell { handle }
        | Event::Pwd { handle, .. }
        | Event::Role { handle, .. }
        | Event::Lagged { handle, .. }
        | Event::Exit { handle, .. }
        | Event::Closed { handle }
        | Event::TermError { handle, .. } => *handle,
        Event::Reconnected { restart, generation } => {
            {
                let mut guard = inner.daemon.lock().expect("daemon mutex poisoned");
                guard.state = DaemonState::Connected {
                    generation: *generation,
                    restarted: true,
                };
                guard.events.push_back(DaemonEvent::Reconnected {
                    restart: *restart,
                    generation: *generation,
                });
            }
            (inner.wake)();
            return;
        }
        Event::Disconnected { restart, message } => {
            {
                let mut guard = inner.daemon.lock().expect("daemon mutex poisoned");
                guard.state = DaemonState::Gone {
                    restart: *restart,
                    message: message.clone(),
                };
                guard.events.push_back(DaemonEvent::Gone {
                    restart: *restart,
                    message: message.clone(),
                });
            }
            {
                let table = inner.routing.lock().expect("routing mutex poisoned");
                for inbox in table.inboxes.values() {
                    inbox.close();
                }
            }
            (inner.wake)();
            return;
        }
        Event::Status(doc) => {
            let sig = status_wake_sig(doc);
            let should_wake = {
                let mut guard = inner.daemon.lock().expect("daemon mutex poisoned");
                // Coalesce queued status snapshots so a sleeping host never accumulates stale docs.
                if let Some(existing) = guard
                    .events
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, DaemonEvent::Status(_)))
                {
                    *existing = DaemonEvent::Status(doc.clone());
                } else {
                    guard.events.push_back(DaemonEvent::Status(doc.clone()));
                }
                let changed = guard.last_status_sig.as_ref() != Some(&sig);
                guard.last_status_sig = Some(sig);
                changed || inner.status_wake_all.load(Ordering::Acquire)
            };
            if should_wake {
                (inner.wake)();
            }
            return;
        }
        Event::UpgradeFailed { message } => {
            {
                let mut guard = inner.daemon.lock().expect("daemon mutex poisoned");
                guard.events.push_back(DaemonEvent::UpgradeFailed {
                    message: message.clone(),
                });
            }
            (inner.wake)();
            return;
        }
    };

    let inbox = {
        let mut table = inner.routing.lock().expect("routing mutex poisoned");
        if let Some(inbox) = table.inboxes.get(&handle) {
            Some(Arc::clone(inbox))
        } else if table.closed_handles.contains(&handle) && table.pending_subscribes == 0 {
            None
        } else {
            if table.early.len() < 64 || table.early.contains_key(&handle) {
                table.early.entry(handle).or_default().push(ev);
            }
            return;
        }
    };

    if let Some(inbox) = inbox {
        let _ = inbox.push(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_wake_sig_ignores_volatile_telemetry_and_detects_structural_changes() {
        let base = serde_json::json!({
            "at": 1000,
            "daemon": { "ns": "probe", "version": "1.5", "instance": "abc", "exiting": false, "brain": { "rss": 6_000_000 }, "snapshots": { "bytes": 0 } },
            "errors": [],
            "terminals": [
                { "id": "t1", "scrollback_rows": 10, "title": "zsh", "pwd": "/tmp", "cwd": "/tmp", "alt": false, "out_bps": 444, "saved_seq": 100 }
            ]
        });
        let volatile_only = serde_json::json!({
            "at": 2000,
            "daemon": { "ns": "probe", "version": "1.5", "instance": "abc", "exiting": false, "brain": { "rss": 6_400_000 }, "snapshots": { "bytes": 16384 } },
            "errors": [],
            "terminals": [
                { "id": "t1", "scrollback_rows": 10, "title": "zsh", "pwd": "/tmp", "cwd": "/tmp", "alt": false, "out_bps": 0, "saved_seq": 425 }
            ]
        });
        assert_eq!(status_wake_sig(&base), status_wake_sig(&volatile_only));

        let with_new_term = serde_json::json!({
            "at": 2000,
            "daemon": { "ns": "probe", "version": "1.5", "instance": "abc", "exiting": false },
            "errors": [],
            "terminals": [
                { "id": "t1", "scrollback_rows": 10, "title": "zsh", "pwd": "/tmp", "cwd": "/tmp", "alt": false },
                { "id": "ext2", "scrollback_rows": 0, "title": "", "pwd": "", "cwd": "/tmp", "alt": false }
            ]
        });
        assert_ne!(status_wake_sig(&base), status_wake_sig(&with_new_term));

        let with_scrollback = serde_json::json!({
            "at": 2000,
            "daemon": { "ns": "probe", "version": "1.5", "instance": "abc", "exiting": false },
            "errors": [],
            "terminals": [
                { "id": "t1", "scrollback_rows": 11, "title": "zsh", "pwd": "/tmp", "cwd": "/tmp", "alt": false }
            ]
        });
        assert_ne!(status_wake_sig(&base), status_wake_sig(&with_scrollback));

        let with_error = serde_json::json!({
            "at": 2000,
            "daemon": { "ns": "probe", "version": "1.5", "instance": "abc", "exiting": false },
            "errors": ["holder error"],
            "terminals": [
                { "id": "t1", "scrollback_rows": 10, "title": "zsh", "pwd": "/tmp", "cwd": "/tmp", "alt": false }
            ]
        });
        assert_ne!(status_wake_sig(&base), status_wake_sig(&with_error));
    }
}
