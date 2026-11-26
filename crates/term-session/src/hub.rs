//! Namespace connection hub: owns `termd-client`, routes per-terminal frames onto dedicated
//! session threads, tracks daemon restarts/disconnects, and spawns/adopts/kills terminals.

use crate::coalesce::CoalescePolicy;
use crate::inbox::SessionInbox;
use crate::session::{run_async_cmd_worker, AsyncCmd, TerminalSession};
use glyph_vt::Theme;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use termd_client::{
    Colors, ConnectOptions, Create, Error, Event, IfExists, Role, Target, TermInfo, Termd,
    TermdHandle, Welcome,
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
}

struct DaemonTracking {
    state: DaemonState,
    events: VecDeque<DaemonEvent>,
}

pub(crate) struct HubInner {
    termd_handle: TermdHandle,
    termd_owner: Mutex<Option<Termd>>,
    routing: Mutex<RoutingTable>,
    daemon: Mutex<DaemonTracking>,
    async_tx: Mutex<Option<mpsc::Sender<AsyncCmd>>>,
    async_worker: Mutex<Option<JoinHandle<()>>>,
    router_thread: Mutex<Option<JoinHandle<()>>>,
    closing: AtomicBool,
    options: HubOptions,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Drop for HubInner {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        {
            let mut tx_guard = self.async_tx.lock().expect("async_tx mutex poisoned");
            *tx_guard = None;
        }
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

    pub(crate) fn send_async(&self, cmd: AsyncCmd) {
        if let Ok(guard) = self.async_tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(cmd);
            }
        }
    }

    pub(crate) fn register_session(&self, handle: u32, inbox: Arc<SessionInbox>) {
        let mut table = self.routing.lock().expect("routing mutex poisoned");
        table.closed_handles.retain(|&h| h != handle);
        if let Some(stashed) = table.early.remove(&handle) {
            for ev in stashed {
                let _ = inbox.push(ev);
            }
        }
        table.inboxes.insert(handle, inbox);
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
        let (async_tx, async_rx) = mpsc::channel();
        let cmd_termd = termd_handle.clone();
        let async_worker = thread::Builder::new()
            .name("term-session-io".into())
            .spawn(move || run_async_cmd_worker(cmd_termd, async_rx))
            .map_err(Error::Io)?;

        let inner = Arc::new(HubInner {
            termd_handle,
            termd_owner: Mutex::new(Some(termd)),
            routing: Mutex::new(RoutingTable {
                inboxes: HashMap::new(),
                early: HashMap::new(),
                closed_handles: VecDeque::with_capacity(128),
            }),
            daemon: Mutex::new(DaemonTracking {
                state: DaemonState::Connected {
                    generation: welcome.generation,
                    restarted: false,
                },
                events: VecDeque::new(),
            }),
            async_tx: Mutex::new(Some(async_tx)),
            async_worker: Mutex::new(Some(async_worker)),
            router_thread: Mutex::new(None),
            closing: AtomicBool::new(false),
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
        let sub = self.inner.termd_handle().subscribe(id, role, size, history_rows)?;
        let inbox = Arc::new(SessionInbox::new(self.inner.options.coalesce.inbox_max_bytes));
        self.inner.register_session(sub.handle, Arc::clone(&inbox));
        match TerminalSession::spawn_worker(
            Arc::clone(&self.inner),
            id.to_owned(),
            sub.handle,
            sub.role,
            sub.cols,
            sub.rows,
            self.inner.options.theme.clone(),
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

    /// Creates (or adopts) a terminal in `g3d-termd`, subscribes to it as [`Role::Own`] at its
    /// size, and returns its running [`TerminalSession`].
    pub fn spawn(&self, opts: &SpawnOptions) -> Result<TerminalSession, Error> {
        let created = self.inner.termd_handle().create(&Create {
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
        })?;
        let target_size = if created.created {
            Some((opts.cols, opts.rows))
        } else {
            None
        };
        self.attach_owner(&created.id, target_size)
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

    /// Kills terminal `id` in `g3d-termd`.
    pub fn kill(&self, id: &str) -> Result<(), Error> {
        self.inner.termd_handle().kill(id)
    }

    /// Fetches the daemon status document once.
    pub fn status(&self) -> Result<serde_json::Value, Error> {
        self.inner.termd_handle().status()
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
            {
                let mut guard = inner.daemon.lock().expect("daemon mutex poisoned");
                guard.events.push_back(DaemonEvent::Status(doc.clone()));
            }
            (inner.wake)();
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
        } else if table.closed_handles.contains(&handle) {
            None
        } else {
            if table.early.len() < 64 {
                table.early.entry(handle).or_default().push(ev);
            }
            return;
        }
    };

    if let Some(inbox) = inbox {
        let _ = inbox.push(ev);
    }
}
