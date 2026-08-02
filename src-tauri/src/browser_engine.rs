//! Dedicated chrome-headless-shell/CDP runtime used by the Codex Browser adapter.
//!
//! Codex's Browser client speaks Chrome DevTools Protocol after discovering a
//! small JSON-RPC backend. The standalone shell cannot inherit the user's normal
//! browser session, and qmux gives each launch a separate, ephemeral profile.

use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const HEADLESS_SHELL_START_TIMEOUT: Duration = Duration::from_secs(8);
const DEFAULT_CDP_TIMEOUT: Duration = Duration::from_secs(30);
const CDP_READ_POLL: Duration = Duration::from_millis(20);
const PROFILE_PREFIX: &str = "qmux-codex-browser-";
const BROWSER_PID_FILE: &str = "QmuxBrowserProcessId";
const BROWSER_EXECUTABLE_FILE: &str = "QmuxBrowserExecutable";
/// Ceiling on remembered fire-and-forget command ids. Chromium answers every
/// command, so the set drains on its own; the cap only stops a wedged socket
/// from growing it without bound.
const MAX_UNWAITED_CDP_IDS: usize = 512;

static PROFILE_NONCE: AtomicU64 = AtomicU64::new(0);

type RpcReply = Result<Value, String>;
type ScreencastSink = Arc<Mutex<Option<mpsc::SyncSender<ScreencastFrame>>>>;

/// One `Page.screencastFrame` payload on its way to the qmux mirror.
///
/// Frames carry a full base64 JPEG, so they bypass the Browser client's event
/// broadcast entirely: copying a Retina-sized image into every connected
/// client's queue would cost far more than the mirror it feeds.
pub struct ScreencastFrame {
    pub tab_id: u64,
    pub data: String,
    pub url: String,
    pub title: String,
}

pub struct BrowserEngine {
    commands: mpsc::Sender<EngineCommand>,
    subscribers: Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    screencast_sink: ScreencastSink,
    next_subscription_id: AtomicU64,
    executable: PathBuf,
}

impl BrowserEngine {
    pub fn start() -> Result<Self, String> {
        let (commands, command_rx) = mpsc::channel();
        let screencast_sink: ScreencastSink = Arc::new(Mutex::new(None));
        let runtime = ChromiumRuntime::launch(command_rx, Arc::clone(&screencast_sink))?;
        let executable = runtime.executable.clone();
        let subscribers = Arc::new(Mutex::new(HashMap::new()));
        let event_subscribers = Arc::clone(&subscribers);

        thread::Builder::new()
            .name("qmux-browser-cdp".to_string())
            .spawn(move || run_engine(runtime, event_subscribers))
            .map_err(|err| format!("failed to start chrome-headless-shell controller: {err}"))?;

        Ok(Self {
            commands,
            subscribers,
            screencast_sink,
            next_subscription_id: AtomicU64::new(1),
            executable,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn subscribe(&self, sender: mpsc::SyncSender<Value>) -> u64 {
        let id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        match self.subscribers.lock() {
            Ok(mut subscribers) => {
                subscribers.insert(id, sender);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(id, sender);
            }
        }
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        match self.subscribers.lock() {
            Ok(mut subscribers) => {
                subscribers.remove(&id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(&id);
            }
        }
    }

    /// Route `Page.screencastFrame` payloads to the qmux mirror instead of the
    /// Browser client event stream. A single sink is enough: the app installs
    /// one pump at startup and every mirrored pane reads from it.
    pub fn set_screencast_sink(&self, sink: Option<mpsc::SyncSender<ScreencastFrame>>) {
        *lock_or_recover(&self.screencast_sink) = sink;
    }

    pub fn call(&self, method: &str, params: Value) -> RpcReply {
        let timeout = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CDP_TIMEOUT)
            .saturating_add(Duration::from_secs(2));
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(EngineCommand::Call {
                method: method.to_string(),
                params,
                reply: reply_tx,
            })
            .map_err(|_| "the qmux chrome-headless-shell controller stopped".to_string())?;
        reply_rx
            .recv_timeout(timeout)
            .map_err(|_| format!("browser method '{method}' timed out"))?
    }
}

enum EngineCommand {
    Call {
        method: String,
        params: Value,
        reply: mpsc::Sender<RpcReply>,
    },
}

struct ChromiumRuntime {
    child: Child,
    profile_dir: PathBuf,
    executable: PathBuf,
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    commands: mpsc::Receiver<EngineCommand>,
    screencast_sink: ScreencastSink,
    screencast_tabs: HashSet<u64>,
    deferred_responses: HashMap<u64, Value>,
    waiting_cdp_ids: HashSet<u64>,
    unwaited_cdp_ids: HashSet<u64>,
    next_cdp_id: u64,
    next_tab_id: u64,
    tabs: HashMap<u64, BrowserTab>,
    target_to_tab: HashMap<String, u64>,
    session_to_tab: HashMap<String, u64>,
    target_sessions: HashMap<(u64, String), String>,
    active_tab: Option<u64>,
    expression_cache: HashMap<String, String>,
}

#[derive(Clone)]
struct BrowserTab {
    id: u64,
    target_id: String,
    url: String,
    title: String,
    attached_session: Option<String>,
}

impl ChromiumRuntime {
    fn launch(
        commands: mpsc::Receiver<EngineCommand>,
        screencast_sink: ScreencastSink,
    ) -> Result<Self, String> {
        cleanup_stale_profile_dirs();
        let executable = find_headless_shell_executable().ok_or_else(|| {
            "chrome-headless-shell was not found; install it with Playwright or set QMUX_CHROME_HEADLESS_SHELL_PATH"
                .to_string()
        })?;
        let profile_dir = unique_profile_dir();
        fs::create_dir_all(&profile_dir).map_err(|err| {
            format!(
                "failed to create chrome-headless-shell profile {}: {err}",
                profile_dir.display()
            )
        })?;
        fs::set_permissions(&profile_dir, fs::Permissions::from_mode(0o700)).map_err(|err| {
            let _ = fs::remove_dir_all(&profile_dir);
            format!(
                "failed to secure chrome-headless-shell profile {}: {err}",
                profile_dir.display()
            )
        })?;

        let mut child = Command::new(&executable)
            .args([
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--window-size=1280,900",
            ])
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                let _ = fs::remove_dir_all(&profile_dir);
                format!("failed to launch {}: {err}", executable.display())
            })?;
        if let Err(err) = write_browser_identity(&profile_dir, child.id(), &executable) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&profile_dir);
            return Err(err);
        }

        let active_port_file = profile_dir.join("DevToolsActivePort");
        let deadline = Instant::now() + HEADLESS_SHELL_START_TIMEOUT;
        let active_port = loop {
            if let Ok(contents) = fs::read_to_string(&active_port_file) {
                let mut lines = contents.lines();
                let port = lines.next().and_then(|line| line.parse::<u16>().ok());
                let websocket_path = lines.next().filter(|line| line.starts_with('/'));
                if let (Some(port), Some(websocket_path)) = (port, websocket_path) {
                    break (port, websocket_path.to_string());
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                let _ = fs::remove_dir_all(&profile_dir);
                return Err(format!(
                    "{} exited before CDP was ready ({status})",
                    executable.display()
                ));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&profile_dir);
                return Err(format!(
                    "timed out waiting for CDP from {}",
                    executable.display()
                ));
            }
            thread::sleep(Duration::from_millis(25));
        };

        let websocket_url = format!("ws://127.0.0.1:{}{}", active_port.0, active_port.1);
        let (mut socket, _) = connect(websocket_url.as_str()).map_err(|err| {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&profile_dir);
            format!("failed to connect to chrome-headless-shell CDP: {err}")
        })?;
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            if let Err(err) = stream.set_read_timeout(Some(CDP_READ_POLL)) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&profile_dir);
                return Err(format!(
                    "failed to configure chrome-headless-shell CDP socket: {err}"
                ));
            }
        }

        Ok(Self {
            child,
            profile_dir,
            executable,
            socket,
            commands,
            screencast_sink,
            screencast_tabs: HashSet::new(),
            deferred_responses: HashMap::new(),
            waiting_cdp_ids: HashSet::new(),
            unwaited_cdp_ids: HashSet::new(),
            next_cdp_id: 1,
            next_tab_id: 1,
            tabs: HashMap::new(),
            target_to_tab: HashMap::new(),
            session_to_tab: HashMap::new(),
            target_sessions: HashMap::new(),
            active_tab: None,
            expression_cache: HashMap::new(),
        })
    }

    fn handle_rpc(
        &mut self,
        method: &str,
        mut params: Value,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        match method {
            "getTabs" => self.get_tabs(subscribers),
            "getUserTabs" | "getUserHistory" => Ok(json!([])),
            "createTab" => self.create_tab(subscribers),
            "focusTab" => {
                let tab_id = tab_id(&params)?;
                let target_id = self.tab(tab_id)?.target_id.clone();
                self.call_cdp(
                    "Target.activateTarget",
                    json!({ "targetId": target_id }),
                    None,
                    DEFAULT_CDP_TIMEOUT,
                    subscribers,
                )?;
                self.active_tab = Some(tab_id);
                Ok(Value::Null)
            }
            "attach" => {
                let tab_id = tab_id(&params)?;
                self.ensure_attached(tab_id, subscribers)?;
                Ok(Value::Null)
            }
            "detach" => self.detach_tab(tab_id(&params)?, subscribers),
            "attachTarget" => {
                let tab_id = tab_id(&params)?;
                let target_id = required_string(&params, "targetId")?;
                self.attach_target(tab_id, &target_id, subscribers)?;
                Ok(Value::Null)
            }
            "detachTarget" => {
                let tab_id = tab_id(&params)?;
                let target_id = required_string(&params, "targetId")?;
                self.detach_target(tab_id, &target_id, subscribers)
            }
            "executeCdp" => self.execute_cdp(&params, subscribers),
            "executeCdpWithCachedExpression" => {
                let cache_key = required_string(&params, "expressionCacheKey")?;
                let command_params = params
                    .get_mut("commandParams")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| "executeCdp requires commandParams".to_string())?;
                if let Some(expression) = command_params
                    .get("expression")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    self.expression_cache.insert(cache_key.clone(), expression);
                } else {
                    let expression =
                        self.expression_cache
                            .get(&cache_key)
                            .cloned()
                            .ok_or_else(|| {
                                format!("cached CDP expression '{cache_key}' is not available")
                            })?;
                    command_params.insert("expression".to_string(), Value::String(expression));
                }
                let result = self.execute_cdp(&params, subscribers)?;
                Ok(json!({ "kind": "executed", "result": result }))
            }
            "markTab" | "nameSession" | "moveMouse" | "turnEnded" => Ok(Value::Null),
            "allowDownload" => {
                Err("qmux automation does not support agent-initiated downloads yet".to_string())
            }
            "finalizeTabs" => self.finalize_tabs(&params, subscribers),
            "claimUserTab" => Err("qmux does not expose tabs from the user's browser".to_string()),
            "executeUnhandledCommand" => Ok(Value::Null),
            _ => Err(format!("No handler registered for method: {method}")),
        }
    }

    fn get_tabs(
        &mut self,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        self.refresh_tabs(subscribers)?;
        let mut tabs = self.tabs.values().cloned().collect::<Vec<_>>();
        tabs.sort_by_key(|tab| tab.id);
        Ok(Value::Array(
            tabs.into_iter().map(|tab| self.tab_json(&tab)).collect(),
        ))
    }

    fn create_tab(
        &mut self,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let result = self.call_cdp(
            "Target.createTarget",
            json!({ "url": "about:blank" }),
            None,
            DEFAULT_CDP_TIMEOUT,
            subscribers,
        )?;
        let target_id = required_string(&result, "targetId")?;
        let tab = self.register_tab(target_id, "about:blank".to_string(), String::new());
        self.active_tab = Some(tab.id);
        Ok(self.tab_json(&tab))
    }

    fn refresh_tabs(
        &mut self,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> Result<(), String> {
        let result = self.call_cdp(
            "Target.getTargets",
            json!({}),
            None,
            DEFAULT_CDP_TIMEOUT,
            subscribers,
        )?;
        let infos = result
            .get("targetInfos")
            .and_then(Value::as_array)
            .ok_or_else(|| "Target.getTargets returned no targetInfos".to_string())?;
        let mut live_targets = HashSet::new();
        for info in infos {
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let target_id = required_string(info, "targetId")?;
            let url = info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            live_targets.insert(target_id.clone());
            if let Some(tab_id) = self.target_to_tab.get(&target_id).copied() {
                if let Some(tab) = self.tabs.get_mut(&tab_id) {
                    tab.url = url;
                    tab.title = title;
                }
            } else {
                self.register_tab(target_id, url, title);
            }
        }
        let removed = self
            .target_to_tab
            .keys()
            .filter(|target_id| !live_targets.contains(*target_id))
            .cloned()
            .collect::<Vec<_>>();
        for target_id in removed {
            if let Some(tab_id) = self.target_to_tab.remove(&target_id) {
                self.tabs.remove(&tab_id);
                self.screencast_tabs.remove(&tab_id);
                self.session_to_tab.retain(|_, owner| *owner != tab_id);
                self.target_sessions
                    .retain(|(owner, _), _| *owner != tab_id);
                if self.active_tab == Some(tab_id) {
                    self.active_tab = None;
                }
            }
        }
        if self.active_tab.is_none() {
            self.active_tab = self.tabs.keys().copied().min();
        }
        Ok(())
    }

    fn register_tab(&mut self, target_id: String, url: String, title: String) -> BrowserTab {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = BrowserTab {
            id,
            target_id: target_id.clone(),
            url,
            title,
            attached_session: None,
        };
        self.target_to_tab.insert(target_id, id);
        self.tabs.insert(id, tab.clone());
        tab
    }

    fn tab_json(&self, tab: &BrowserTab) -> Value {
        json!({
            "id": tab.id,
            "url": tab.url,
            "title": tab.title,
            "active": self.active_tab == Some(tab.id)
        })
    }

    fn tab(&self, tab_id: u64) -> Result<&BrowserTab, String> {
        self.tabs
            .get(&tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist"))
    }

    fn ensure_attached(
        &mut self,
        tab_id: u64,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> Result<String, String> {
        if let Some(session) = self.tab(tab_id)?.attached_session.clone() {
            return Ok(session);
        }
        let target_id = self.tab(tab_id)?.target_id.clone();
        let result = self.call_cdp(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            DEFAULT_CDP_TIMEOUT,
            subscribers,
        )?;
        let session_id = required_string(&result, "sessionId")?;
        self.session_to_tab.insert(session_id.clone(), tab_id);
        self.tabs
            .get_mut(&tab_id)
            .expect("tab disappeared while attaching")
            .attached_session = Some(session_id.clone());
        Ok(session_id)
    }

    fn detach_tab(
        &mut self,
        tab_id: u64,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let Some(session_id) = self.tab(tab_id)?.attached_session.clone() else {
            return Ok(Value::Null);
        };
        self.call_cdp(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
            None,
            DEFAULT_CDP_TIMEOUT,
            subscribers,
        )?;
        if self.tab(tab_id)?.attached_session.as_deref() == Some(session_id.as_str()) {
            self.forget_session(&session_id);
            broadcast_cdp_detach(subscribers, tab_id);
        }
        Ok(Value::Null)
    }

    fn attach_target(
        &mut self,
        tab_id: u64,
        target_id: &str,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> Result<String, String> {
        if let Some(session) = self
            .target_sessions
            .get(&(tab_id, target_id.to_string()))
            .cloned()
        {
            return Ok(session);
        }
        self.tab(tab_id)?;
        let result = self.call_cdp(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            DEFAULT_CDP_TIMEOUT,
            subscribers,
        )?;
        let session_id = required_string(&result, "sessionId")?;
        self.session_to_tab.insert(session_id.clone(), tab_id);
        self.target_sessions
            .insert((tab_id, target_id.to_string()), session_id.clone());
        Ok(session_id)
    }

    fn detach_target(
        &mut self,
        tab_id: u64,
        target_id: &str,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let Some(session_id) = self
            .target_sessions
            .remove(&(tab_id, target_id.to_string()))
        else {
            return Ok(Value::Null);
        };
        self.call_cdp(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
            None,
            DEFAULT_CDP_TIMEOUT,
            subscribers,
        )?;
        self.session_to_tab.remove(&session_id);
        Ok(Value::Null)
    }

    fn execute_cdp(
        &mut self,
        params: &Value,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let target = params
            .get("target")
            .ok_or_else(|| "executeCdp requires target".to_string())?;
        let tab_id = tab_id(target)?;
        let method = required_string(params, "method")?;
        let command_params = params
            .get("commandParams")
            .cloned()
            .unwrap_or_else(|| json!({}));
        validate_cdp_command(&method, &command_params)?;
        let timeout = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CDP_TIMEOUT);
        let session_id = if let Some(session_id) = target.get("sessionId").and_then(Value::as_str) {
            if self.session_to_tab.get(session_id).copied() != Some(tab_id) {
                return Err(format!(
                    "CDP session {session_id} is not attached to tab {tab_id}"
                ));
            }
            session_id.to_string()
        } else if let Some(target_id) = target.get("targetId").and_then(Value::as_str) {
            self.attach_target(tab_id, target_id, subscribers)?
        } else {
            self.ensure_attached(tab_id, subscribers)?
        };
        // Mark a starting stream before sending the command: Chromium may put
        // its first Page.screencastFrame on the socket before the command
        // response, and that frame must be acknowledged and routed rather than
        // leaking into the Browser client's ordinary event feed.
        let starting_screencast = method == "Page.startScreencast";
        let was_screencasting = starting_screencast && !self.screencast_tabs.insert(tab_id);
        let result = self.call_cdp(
            &method,
            command_params,
            Some(&session_id),
            timeout,
            subscribers,
        );
        if result.is_err() && starting_screencast && !was_screencasting {
            self.screencast_tabs.remove(&tab_id);
        }
        let result = result?;
        // Remember which tabs stream so their frames are routed to the mirror
        // (and acknowledged here) instead of being broadcast as ordinary CDP
        // events. qmux is the only screencast caller; a Browser client that
        // started one would find its frames consumed by the mirror pump.
        match method.as_str() {
            "Page.stopScreencast" => {
                self.screencast_tabs.remove(&tab_id);
            }
            _ => {}
        }
        Ok(result)
    }

    fn finalize_tabs(
        &mut self,
        params: &Value,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let keep = params
            .get("keep")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| {
                        value
                            .as_u64()
                            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                    })
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let close = self
            .tabs
            .keys()
            .copied()
            .filter(|tab_id| !keep.contains(tab_id))
            .collect::<Vec<_>>();
        for tab_id in close {
            let target_id = self.tab(tab_id)?.target_id.clone();
            self.call_cdp(
                "Target.closeTarget",
                json!({ "targetId": target_id }),
                None,
                DEFAULT_CDP_TIMEOUT,
                subscribers,
            )?;
        }
        self.refresh_tabs(subscribers)?;
        Ok(Value::Null)
    }

    fn handle_next_queued_command(
        &mut self,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> Result<bool, String> {
        let command = match self.commands.try_recv() {
            Ok(command) => command,
            Err(mpsc::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("the qmux chrome-headless-shell command queue closed".to_string());
            }
        };
        match command {
            EngineCommand::Call {
                method,
                params,
                reply,
            } => {
                let result = self.handle_rpc(&method, params, subscribers);
                let _ = reply.send(result);
            }
        }
        Ok(true)
    }

    fn call_cdp(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let id = self.next_cdp_id;
        self.next_cdp_id += 1;
        let mut command = Map::new();
        command.insert("id".to_string(), json!(id));
        command.insert("method".to_string(), json!(method));
        command.insert("params".to_string(), params);
        if let Some(session_id) = session_id {
            command.insert("sessionId".to_string(), json!(session_id));
        }
        self.socket
            .send(Message::text(Value::Object(command).to_string()))
            .map_err(|err| format!("failed to send CDP method {method}: {err}"))?;

        let started_at = Instant::now();
        let session_label = session_id.unwrap_or("browser");
        eprintln!(
            "qmux: CDP send id={id} method={method} session={session_label} timeout_ms={}",
            timeout.as_millis()
        );
        self.waiting_cdp_ids.insert(id);
        let result =
            self.wait_for_cdp_response(id, method, session_id, timeout, started_at, subscribers);
        self.waiting_cdp_ids.remove(&id);
        self.deferred_responses.remove(&id);
        result
    }

    /// Send a CDP command whose reply carries nothing worth waiting for.
    ///
    /// Screencast acknowledgements arrive at frame rate and gate the next
    /// frame, so blocking the controller on each one would cost more latency
    /// than the frame is worth. The id is remembered only so the eventual
    /// reply isn't reported as an orphaned response.
    fn send_cdp_without_waiting(&mut self, method: &str, params: Value, session_id: Option<&str>) {
        let id = self.next_cdp_id;
        self.next_cdp_id += 1;
        let mut command = Map::new();
        command.insert("id".to_string(), json!(id));
        command.insert("method".to_string(), json!(method));
        command.insert("params".to_string(), params);
        if let Some(session_id) = session_id {
            command.insert("sessionId".to_string(), json!(session_id));
        }
        if let Err(err) = self
            .socket
            .send(Message::text(Value::Object(command).to_string()))
        {
            eprintln!("qmux: CDP send id={id} method={method} status=error detail={err}");
            return;
        }
        if self.unwaited_cdp_ids.len() >= MAX_UNWAITED_CDP_IDS {
            // Ids only grow, so keeping the newest half discards exactly the
            // replies old enough that Chromium is never going to send them.
            let mut ids = self.unwaited_cdp_ids.iter().copied().collect::<Vec<_>>();
            ids.sort_unstable();
            let dropped = ids.len() / 2;
            for stale in ids.into_iter().take(dropped) {
                self.unwaited_cdp_ids.remove(&stale);
            }
        }
        self.unwaited_cdp_ids.insert(id);
    }

    /// True when `id` answers a command sent with `send_cdp_without_waiting`.
    fn take_unwaited_cdp_id(&mut self, id: u64) -> bool {
        self.unwaited_cdp_ids.remove(&id)
    }

    fn wait_for_cdp_response(
        &mut self,
        id: u64,
        method: &str,
        session_id: Option<&str>,
        timeout: Duration,
        started_at: Instant,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> RpcReply {
        let session_label = session_id.unwrap_or("browser");
        let deadline = Instant::now() + timeout;
        loop {
            let message = if let Some(message) = self.deferred_responses.remove(&id) {
                Ok(Some(message))
            } else {
                // Browser clients may need to answer a CDP event before this command
                // can finish (for example Fetch.requestPaused during Page.navigate).
                // Service one queued command between socket reads so that response
                // can be sent without abandoning the command currently in flight.
                self.handle_next_queued_command(subscribers)?;
                if let Some(message) = self.deferred_responses.remove(&id) {
                    Ok(Some(message))
                } else {
                    self.read_cdp_message(subscribers)
                }
            };
            match message {
                Ok(Some(message)) if message.get("id").and_then(Value::as_u64) == Some(id) => {
                    let response_session = message
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("browser");
                    if let Some(error) = message.get("error") {
                        let detail = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown CDP error");
                        eprintln!(
                            "qmux: CDP receive id={id} method={method} session={response_session} expected_session={session_label} status=error elapsed_ms={} detail={detail}",
                            started_at.elapsed().as_millis()
                        );
                        if detail.contains("Session with given id not found")
                            || detail.contains("session not found")
                        {
                            if let Some(session_id) = session_id {
                                if let Some(tab_id) = self.forget_session(session_id) {
                                    broadcast_cdp_detach(subscribers, tab_id);
                                }
                            }
                            return Err(format!("Debugger is not attached: {detail}"));
                        }
                        return Err(format!("{method}: {detail}"));
                    }
                    eprintln!(
                        "qmux: CDP receive id={id} method={method} session={response_session} expected_session={session_label} status=ok elapsed_ms={}",
                        started_at.elapsed().as_millis()
                    );
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Ok(Some(message)) => {
                    // A reply to a screencast acknowledgement answers nobody and
                    // is not late — take_unwaited_cdp_id filters those out.
                    if let Some(unexpected_id) = message.get("id").and_then(Value::as_u64)
                        && !self.take_unwaited_cdp_id(unexpected_id)
                    {
                        let response_session = message
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or("browser");
                        if self.waiting_cdp_ids.contains(&unexpected_id) {
                            eprintln!(
                                "qmux: CDP defer id={unexpected_id} session={response_session} while_waiting_for_id={id} method={method} session={session_label} elapsed_ms={}",
                                started_at.elapsed().as_millis()
                            );
                            defer_cdp_response(&mut self.deferred_responses, message);
                        } else {
                            eprintln!(
                                "qmux: CDP discard late response id={unexpected_id} session={response_session} while_waiting_for_id={id} method={method} session={session_label} elapsed_ms={}",
                                started_at.elapsed().as_millis()
                            );
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!(
                        "qmux: CDP read-error id={id} method={method} session={session_label} elapsed_ms={} detail={err}",
                        started_at.elapsed().as_millis()
                    );
                    return Err(err);
                }
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "qmux: CDP timeout id={id} method={method} session={session_label} elapsed_ms={}",
                    started_at.elapsed().as_millis()
                );
                return Err(format!("CDP method {method} timed out"));
            }
        }
    }

    fn read_cdp_message(
        &mut self,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) -> Result<Option<Value>, String> {
        let message = match self.socket.read() {
            Ok(Message::Text(text)) => serde_json::from_str::<Value>(text.as_str())
                .map_err(|err| format!("chrome-headless-shell sent invalid CDP JSON: {err}"))?,
            Ok(Message::Ping(payload)) => {
                self.socket.send(Message::Pong(payload)).map_err(|err| {
                    format!("failed to answer chrome-headless-shell CDP ping: {err}")
                })?;
                return Ok(None);
            }
            Ok(Message::Close(_)) => {
                return Err("chrome-headless-shell closed its CDP connection".to_string());
            }
            Ok(_) => return Ok(None),
            Err(tungstenite::Error::Io(err))
                if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(format!("failed to read chrome-headless-shell CDP: {err}")),
        };

        if message.get("method").is_some() {
            self.handle_cdp_event(&message, subscribers);
        }
        Ok(Some(message))
    }

    fn handle_cdp_event(
        &mut self,
        message: &Value,
        subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    ) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let outer_session = message.get("sessionId").and_then(Value::as_str);

        if method == "Target.targetInfoChanged" {
            if let Some(info) = params.get("targetInfo") {
                if let Some(target_id) = info.get("targetId").and_then(Value::as_str) {
                    if let Some(tab_id) = self.target_to_tab.get(target_id).copied() {
                        if let Some(tab) = self.tabs.get_mut(&tab_id) {
                            tab.url = info
                                .get("url")
                                .and_then(Value::as_str)
                                .unwrap_or(&tab.url)
                                .to_string();
                            tab.title = info
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or(&tab.title)
                                .to_string();
                        }
                    }
                }
            }
        }

        if method == "Target.detachedFromTarget" && outer_session.is_none() {
            if let Some(detached_session) = params.get("sessionId").and_then(Value::as_str)
                && let Some(tab_id) = self.forget_session(detached_session)
            {
                broadcast_cdp_detach(subscribers, tab_id);
            }
            return;
        }

        let Some(tab_id) =
            outer_session.and_then(|session| self.session_to_tab.get(session).copied())
        else {
            return;
        };
        if method == "Page.screencastFrame" && self.screencast_tabs.contains(&tab_id) {
            self.deliver_screencast_frame(tab_id, outer_session, &params);
            return;
        }
        if method == "Target.attachedToTarget" {
            if let Some(nested_session) = params.get("sessionId").and_then(Value::as_str) {
                self.session_to_tab
                    .insert(nested_session.to_string(), tab_id);
                if let Some(target_id) = params
                    .pointer("/targetInfo/targetId")
                    .and_then(Value::as_str)
                {
                    self.target_sessions
                        .insert((tab_id, target_id.to_string()), nested_session.to_string());
                }
            }
        } else if method == "Target.detachedFromTarget" {
            if let Some(nested_session) = params.get("sessionId").and_then(Value::as_str) {
                self.session_to_tab.remove(nested_session);
                self.target_sessions
                    .retain(|_, session| session != nested_session);
            }
        }
        let is_top_level = self
            .tabs
            .get(&tab_id)
            .and_then(|tab| tab.attached_session.as_deref())
            == outer_session;
        let source = if is_top_level {
            json!({ "tabId": tab_id })
        } else {
            json!({ "tabId": tab_id, "sessionId": outer_session })
        };
        broadcast(
            subscribers,
            json!({
                "jsonrpc": "2.0",
                "method": "onCDPEvent",
                "params": {
                    "source": source,
                    "method": method,
                    "params": params
                }
            }),
        );
    }

    /// Acknowledge a screencast frame and hand it to the mirror pump.
    ///
    /// Chromium keeps only a couple of frames in flight and withholds the next
    /// one until the current frame is acknowledged, so the ack goes out before
    /// anything else — including before the sink is consulted, so a mirror that
    /// closed mid-frame can't wedge the stream for the tab it left behind.
    fn deliver_screencast_frame(&mut self, tab_id: u64, session_id: Option<&str>, params: &Value) {
        if let Some(frame_session) = params.get("sessionId").and_then(Value::as_u64) {
            self.send_cdp_without_waiting(
                "Page.screencastFrameAck",
                json!({ "sessionId": frame_session }),
                session_id,
            );
        }
        let sink = lock_or_recover(&self.screencast_sink).clone();
        let (Some(sink), Some(data)) = (sink, params.get("data").and_then(Value::as_str)) else {
            return;
        };
        let tab = self.tabs.get(&tab_id);
        let frame = ScreencastFrame {
            tab_id,
            data: data.to_string(),
            url: tab.map(|tab| tab.url.clone()).unwrap_or_default(),
            title: tab.map(|tab| tab.title.clone()).unwrap_or_default(),
        };
        // A mirror that can't keep up should skip frames rather than queue
        // full-resolution JPEGs it will only render stale.
        if let Err(mpsc::TrySendError::Disconnected(_)) = sink.try_send(frame) {
            *lock_or_recover(&self.screencast_sink) = None;
        }
    }

    fn forget_session(&mut self, session_id: &str) -> Option<u64> {
        let tab_id = self.session_to_tab.remove(session_id)?;
        if self
            .tabs
            .get(&tab_id)
            .and_then(|tab| tab.attached_session.as_deref())
            == Some(session_id)
        {
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
                tab.attached_session = None;
            }
        }
        self.target_sessions
            .retain(|_, attached_session| attached_session != session_id);
        Some(tab_id)
    }
}

impl Drop for ChromiumRuntime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.profile_dir);
    }
}

fn run_engine(
    mut runtime: ChromiumRuntime,
    subscribers: Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
) {
    loop {
        match runtime.handle_next_queued_command(&subscribers) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(_) => return,
        }
        match runtime.read_cdp_message(&subscribers) {
            Ok(Some(message)) => {
                if let Some(id) = message.get("id").and_then(Value::as_u64)
                    && !runtime.take_unwaited_cdp_id(id)
                {
                    eprintln!("qmux: CDP discard late response id={id} with no command waiting");
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("qmux: chrome-headless-shell CDP controller stopped: {err}");
                return;
            }
        }
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn broadcast(subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>, event: Value) {
    let mut subscribers = match subscribers.lock() {
        Ok(subscribers) => subscribers,
        Err(poisoned) => poisoned.into_inner(),
    };
    subscribers.retain(|_, subscriber| match subscriber.try_send(event.clone()) {
        Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    });
}

fn broadcast_cdp_detach(
    subscribers: &Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    tab_id: u64,
) {
    broadcast(
        subscribers,
        json!({
            "jsonrpc": "2.0",
            "method": "onCDPDetach",
            "params": { "tabId": tab_id }
        }),
    );
}

fn validate_cdp_command(method: &str, params: &Value) -> Result<(), String> {
    match method {
        "Browser.setDownloadBehavior" | "Page.setDownloadBehavior" => {
            return Err(
                "qmux automation does not support agent-initiated downloads yet".to_string(),
            );
        }
        "DOM.setFileInputFiles" => {
            return Err(
                "qmux automation does not support agent-selected local file uploads yet"
                    .to_string(),
            );
        }
        "Page.navigate" => {}
        _ => return Ok(()),
    }
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Page.navigate requires a URL".to_string())?;
    if url == "about:blank" || url.starts_with("http://") || url.starts_with("https://") {
        return Ok(());
    }
    Err("qmux automation only navigates to http(s) URLs".to_string())
}

fn find_headless_shell_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QMUX_CHROME_HEADLESS_SHELL_PATH") {
        let path = PathBuf::from(path);
        if is_executable_file(&path) {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(executable_dir) = current_exe.parent()
    {
        candidates.push(executable_dir.join("chrome-headless-shell"));
        candidates.push(executable_dir.join("headless_shell"));
        candidates.push(executable_dir.join("../Resources/chrome-headless-shell"));
        candidates
            .push(executable_dir.join("../Resources/chrome-headless-shell/chrome-headless-shell"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            candidates.push(directory.join("chrome-headless-shell"));
            candidates.push(directory.join("headless_shell"));
        }
    }
    if let Some(candidate) = candidates.into_iter().find(|path| is_executable_file(path)) {
        return Some(candidate);
    }

    let mut playwright_caches = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .filter(|path| path != "0")
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(cache) = dirs::cache_dir() {
        playwright_caches.push(cache.join("ms-playwright"));
    }
    playwright_caches
        .into_iter()
        .find_map(|cache| find_playwright_headless_shell(&cache))
}

fn find_playwright_headless_shell(cache: &Path) -> Option<PathBuf> {
    let mut installs = fs::read_dir(cache)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let revision = entry
                .file_name()
                .to_str()?
                .strip_prefix("chromium_headless_shell-")?
                .parse::<u64>()
                .ok()?;
            entry
                .file_type()
                .ok()?
                .is_dir()
                .then_some((revision, entry.path()))
        })
        .collect::<Vec<_>>();
    installs.sort_unstable_by_key(|(revision, _)| std::cmp::Reverse(*revision));
    installs.into_iter().find_map(|(_, install)| {
        find_file_below(&install, "chrome-headless-shell", 2)
            .or_else(|| find_file_below(&install, "headless_shell", 2))
    })
}

fn find_file_below(directory: &Path, filename: &str, remaining_depth: usize) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if entry.file_name() == filename && is_executable_file(&path) {
            return Some(path);
        }
        if remaining_depth > 0
            && file_type.is_dir()
            && let Some(found) = find_file_below(&path, filename, remaining_depth - 1)
        {
            return Some(found);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn unique_profile_dir() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = PROFILE_NONCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{PROFILE_PREFIX}{}-{timestamp}-{nonce}",
        std::process::id()
    ))
}

fn write_browser_identity(profile_dir: &Path, pid: u32, executable: &Path) -> Result<(), String> {
    fs::write(profile_dir.join(BROWSER_PID_FILE), pid.to_string()).map_err(|err| {
        format!(
            "failed to record chrome-headless-shell pid in {}: {err}",
            profile_dir.display()
        )
    })?;
    fs::write(
        profile_dir.join(BROWSER_EXECUTABLE_FILE),
        executable.as_os_str().as_encoded_bytes(),
    )
    .map_err(|err| {
        format!(
            "failed to record chrome-headless-shell executable in {}: {err}",
            profile_dir.display()
        )
    })
}

fn cleanup_stale_profile_dirs() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let profile_dir = entry.path();
        let Some(owner_pid) = profile_owner_pid(&profile_dir) else {
            continue;
        };
        if process_is_alive(owner_pid) {
            continue;
        }

        let browser_pid = read_pid(&profile_dir.join(BROWSER_PID_FILE));
        let expected_executable = fs::read(profile_dir.join(BROWSER_EXECUTABLE_FILE))
            .ok()
            .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes)));
        let close_sent = close_browser_over_cdp(&profile_dir);

        if let Some(browser_pid) = browser_pid {
            wait_for_process_exit(browser_pid, Duration::from_millis(500));
            if process_is_alive(browser_pid)
                && expected_executable.as_deref().is_some_and(|expected| {
                    process_executable(browser_pid).as_deref() == Some(expected)
                })
            {
                // The profile records both the pid and exact executable path. Only
                // signal after validating both so a recycled pid cannot kill an
                // unrelated process.
                unsafe {
                    libc::kill(browser_pid as libc::pid_t, libc::SIGTERM);
                }
                wait_for_process_exit(browser_pid, Duration::from_millis(500));
            }
        }

        if close_sent || browser_pid.is_none_or(|pid| !process_is_alive(pid)) {
            if let Err(err) = fs::remove_dir_all(&profile_dir) {
                eprintln!(
                    "qmux: failed to remove stale browser profile {}: {err}",
                    profile_dir.display()
                );
            } else {
                eprintln!(
                    "qmux: reclaimed stale chrome-headless-shell profile {}",
                    profile_dir.display()
                );
            }
        }
    }
}

fn profile_owner_pid(profile_dir: &Path) -> Option<u32> {
    profile_dir
        .file_name()?
        .to_str()?
        .strip_prefix(PROFILE_PREFIX)?
        .split('-')
        .next()?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0 && *pid <= libc::pid_t::MAX as u32)
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0 && *pid <= libc::pid_t::MAX as u32)
}

fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_for_process_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while process_is_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::pid_t,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    (length > 0).then(|| {
        PathBuf::from(std::ffi::OsString::from_vec(
            buffer[..length as usize].to_vec(),
        ))
    })
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

fn close_browser_over_cdp(profile_dir: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(profile_dir.join("DevToolsActivePort")) else {
        return false;
    };
    let mut lines = contents.lines();
    let Some(port) = lines.next().and_then(|line| line.parse::<u16>().ok()) else {
        return false;
    };
    let Some(websocket_path) = lines.next().filter(|line| line.starts_with('/')) else {
        return false;
    };
    let url = format!("ws://127.0.0.1:{port}{websocket_path}");
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let Ok((mut socket, _)) = tungstenite::client(url.as_str(), stream) else {
        return false;
    };
    socket
        .send(Message::text(
            json!({ "id": 1, "method": "Browser.close" }).to_string(),
        ))
        .is_ok()
}

fn tab_id(value: &Value) -> Result<u64, String> {
    value
        .get("tabId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| "browser method requires a positive numeric tabId".to_string())
}

fn defer_cdp_response(responses: &mut HashMap<u64, Value>, message: Value) -> Option<u64> {
    let id = message.get("id").and_then(Value::as_u64)?;
    responses.insert(id, message);
    Some(id)
}

fn required_string(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("browser method requires '{field}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    #[test]
    fn tab_ids_must_be_positive_numbers() {
        assert_eq!(tab_id(&json!({ "tabId": 3 })).unwrap(), 3);
        assert!(tab_id(&json!({ "tabId": 0 })).is_err());
        assert!(tab_id(&json!({ "tabId": "3" })).is_err());
    }

    #[test]
    fn out_of_order_cdp_responses_are_buffered_by_id() {
        let mut responses = HashMap::new();
        let message = json!({ "id": 17, "result": { "ok": true } });

        assert_eq!(
            defer_cdp_response(&mut responses, message.clone()),
            Some(17)
        );
        assert_eq!(responses.remove(&17), Some(message));
        assert_eq!(
            defer_cdp_response(&mut responses, json!({ "method": "Page.loadEventFired" })),
            None
        );
        assert!(responses.is_empty());
    }

    #[test]
    fn profile_paths_are_unique_and_scoped_to_temp() {
        let first = unique_profile_dir();
        let second = unique_profile_dir();
        assert_ne!(first, second);
        assert!(first.starts_with(std::env::temp_dir()));
        assert_eq!(profile_owner_pid(&first), Some(std::process::id()));
    }

    #[test]
    fn profile_owner_pid_rejects_unowned_or_unsafe_names() {
        let temp = std::env::temp_dir();
        assert_eq!(
            profile_owner_pid(&temp.join("qmux-codex-browser-42-123-0")),
            Some(42)
        );
        assert_eq!(
            profile_owner_pid(&temp.join("other-browser-42-123-0")),
            None
        );
        assert_eq!(
            profile_owner_pid(&temp.join("qmux-codex-browser-0-123-0")),
            None
        );
        assert_eq!(
            profile_owner_pid(&temp.join("qmux-codex-browser-4294967295-123-0")),
            None
        );
    }

    #[test]
    fn browser_identity_round_trips_pid_and_executable() {
        let profile = unique_profile_dir();
        fs::create_dir_all(&profile).unwrap();
        let executable = Path::new("/tmp/chrome headless shell");
        write_browser_identity(&profile, 42, executable).unwrap();

        assert_eq!(read_pid(&profile.join(BROWSER_PID_FILE)), Some(42));
        let recorded = fs::read(profile.join(BROWSER_EXECUTABLE_FILE)).unwrap();
        assert_eq!(recorded, executable.as_os_str().as_encoded_bytes());
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn playwright_cache_prefers_newest_executable_headless_shell() {
        let cache = unique_profile_dir();
        for revision in [1200, 1223, 1300] {
            let install = cache
                .join(format!("chromium_headless_shell-{revision}"))
                .join("chrome-headless-shell-test-arch");
            fs::create_dir_all(&install).unwrap();
            let executable = install.join("chrome-headless-shell");
            fs::write(&executable, []).unwrap();
            let mode = if revision == 1300 { 0o600 } else { 0o700 };
            fs::set_permissions(&executable, fs::Permissions::from_mode(mode)).unwrap();
        }

        let selected = find_playwright_headless_shell(&cache).unwrap();
        assert!(selected.starts_with(cache.join("chromium_headless_shell-1223")));
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn navigation_rejects_local_and_active_content_schemes() {
        assert!(
            validate_cdp_command("Page.navigate", &json!({ "url": "https://example.com" })).is_ok()
        );
        assert!(
            validate_cdp_command("Page.navigate", &json!({ "url": "http://localhost:3000" }))
                .is_ok()
        );
        assert!(validate_cdp_command("Page.navigate", &json!({ "url": "about:blank" })).is_ok());
        assert!(
            validate_cdp_command("Page.navigate", &json!({ "url": "file:///etc/passwd" })).is_err()
        );
        assert!(
            validate_cdp_command("Page.navigate", &json!({ "url": "javascript:alert(1)" }))
                .is_err()
        );
    }

    #[test]
    fn cdp_cannot_read_or_write_arbitrary_local_paths() {
        assert!(
            validate_cdp_command(
                "Browser.setDownloadBehavior",
                &json!({ "behavior": "allow", "downloadPath": "/tmp" })
            )
            .is_err()
        );
        assert!(
            validate_cdp_command(
                "DOM.setFileInputFiles",
                &json!({ "files": ["/etc/passwd"] })
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "launches an installed chrome-headless-shell"]
    fn headless_shell_supports_core_tab_and_runtime_commands() {
        let engine = BrowserEngine::start().unwrap();
        let tab = engine.call("createTab", json!({})).unwrap();
        let tab_id = tab["id"].as_u64().unwrap();
        engine.call("attach", json!({ "tabId": tab_id })).unwrap();
        let result = engine
            .call(
                "executeCdp",
                json!({
                    "target": { "tabId": tab_id },
                    "method": "Runtime.evaluate",
                    "commandParams": {
                        "expression": "6 * 7",
                        "returnByValue": true
                    }
                }),
            )
            .unwrap();
        assert_eq!(result["result"]["value"], 42);
    }

    #[test]
    #[ignore = "launches an installed chrome-headless-shell"]
    fn paused_navigation_services_continue_request_while_navigate_waits() {
        let engine = Arc::new(BrowserEngine::start().unwrap());
        let tab = engine.call("createTab", json!({})).unwrap();
        let tab_id = tab["id"].as_u64().unwrap();
        engine.call("attach", json!({ "tabId": tab_id })).unwrap();
        engine
            .call(
                "executeCdp",
                json!({
                    "target": { "tabId": tab_id },
                    "method": "Page.enable",
                    "commandParams": {}
                }),
            )
            .unwrap();
        engine
            .call(
                "executeCdp",
                json!({
                    "target": { "tabId": tab_id },
                    "method": "Fetch.enable",
                    "commandParams": {}
                }),
            )
            .unwrap();

        let (event_tx, event_rx) = mpsc::sync_channel(32);
        let subscription = engine.subscribe(event_tx);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });

        let navigation_engine = Arc::clone(&engine);
        let navigation = thread::spawn(move || {
            navigation_engine.call(
                "executeCdp",
                json!({
                    "target": { "tabId": tab_id },
                    "method": "Page.navigate",
                    "commandParams": { "url": format!("http://{address}/") },
                    "timeoutMs": 5_000
                }),
            )
        });

        let request_id = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if event.pointer("/params/method").and_then(Value::as_str)
                == Some("Fetch.requestPaused")
            {
                break event
                    .pointer("/params/params/requestId")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string();
            }
        };
        engine
            .call(
                "executeCdp",
                json!({
                    "target": { "tabId": tab_id },
                    "method": "Fetch.continueRequest",
                    "commandParams": { "requestId": request_id },
                    "timeoutMs": 5_000
                }),
            )
            .unwrap();

        navigation.join().unwrap().unwrap();
        server.join().unwrap();
        engine.unsubscribe(subscription);
    }

    /// Width and height from a JPEG's start-of-frame marker, so a captured
    /// frame can be measured without pulling in an image decoder.
    fn jpeg_dimensions(bytes: &[u8]) -> Option<(u16, u16)> {
        let mut index = 2;
        while index + 8 < bytes.len() {
            if bytes[index] != 0xFF {
                index += 1;
                continue;
            }
            let marker = bytes[index + 1];
            let length = usize::from(u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]));
            // Every SOFn frame header carries the dimensions; the other 0xC*
            // markers (Huffman tables, arithmetic conditioning, restart) do not.
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]);
                let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]);
                return Some((width, height));
            }
            index += 2 + length;
        }
        None
    }

    #[test]
    fn jpeg_dimensions_are_read_from_the_start_of_frame_marker() {
        let jpeg = [
            0xFF, 0xD8, // SOI
            0xFF, 0xC4, 0x00, 0x04, 0x00, 0x00, // a Huffman table to skip over
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x07, 0x08, 0x0A, 0x00, // SOF0: 2560x1800
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((2560, 1800)));
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8]), None);
    }

    /// Chromium's screencast only ever scales frames *down* (its scale factor
    /// starts at 1 and `maxWidth`/`maxHeight` can only shrink it), so frames
    /// arrive at CSS resolution no matter what device scale is emulated. This
    /// pins both halves of that: the stream stays at 1x, and a screenshot taken
    /// against the same override is the 2x image the mirror settles on.
    #[test]
    #[ignore = "launches an installed chrome-headless-shell"]
    fn screencast_frames_stream_at_css_resolution_and_settle_at_display_scale() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let engine = BrowserEngine::start().unwrap();
        let (frames_tx, frames_rx) = mpsc::sync_channel(4);
        engine.set_screencast_sink(Some(frames_tx));
        let (event_tx, event_rx) = mpsc::sync_channel(64);
        let subscription = engine.subscribe(event_tx);

        let tab = engine.call("createTab", json!({})).unwrap();
        let tab_id = tab["id"].as_u64().unwrap();
        engine.call("attach", json!({ "tabId": tab_id })).unwrap();
        let execute = |method: &str, params: Value| {
            engine
                .call(
                    "executeCdp",
                    json!({
                        "target": { "tabId": tab_id },
                        "method": method,
                        "commandParams": params,
                        "timeoutMs": 10_000
                    }),
                )
                .unwrap()
        };
        execute("Page.enable", json!({}));
        execute(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": 640,
                "height": 480,
                "deviceScaleFactor": 2.0,
                "mobile": false,
                "screenWidth": 640,
                "screenHeight": 480
            }),
        );
        execute(
            "Page.startScreencast",
            json!({
                "format": "jpeg",
                "quality": 80,
                "maxWidth": 1280,
                "maxHeight": 960,
                "everyNthFrame": 1
            }),
        );
        let mut frames = Vec::new();

        // Chromium withholds frames once a couple are outstanding, so getting
        // past the second one proves the controller is acknowledging them.
        for repaint in 0..8 {
            if frames.len() >= 3 {
                break;
            }
            execute(
                "Runtime.evaluate",
                json!({
                    "expression": format!(
                        "document.body.style.background = 'rgb({repaint}, 40, 90)'"
                    )
                }),
            );
            if let Ok(frame) = frames_rx.recv_timeout(Duration::from_secs(5)) {
                frames.push(frame);
            }
        }
        assert!(
            frames.len() >= 3,
            "expected the screencast to keep streaming, got {} frame(s)",
            frames.len()
        );
        assert!(frames.iter().all(|frame| frame.tab_id == tab_id));

        let streamed = STANDARD.decode(&frames[0].data).unwrap();
        assert_eq!(
            jpeg_dimensions(&streamed),
            Some((640, 480)),
            "screencast frames stay at CSS resolution however the display scales"
        );

        // Frames belong to the mirror, not to the Browser client's event feed.
        while let Ok(event) = event_rx.try_recv() {
            assert_ne!(
                event.pointer("/params/method").and_then(Value::as_str),
                Some("Page.screencastFrame")
            );
        }

        // ...which is why the mirror settles on a screenshot: the same override
        // captures at the emulated 2x, so the resting mirror is Retina-sharp
        // even though nothing streamed to it ever is.
        let settled = execute(
            "Page.captureScreenshot",
            json!({
                "format": "jpeg",
                "quality": 80,
                "fromSurface": true,
                "captureBeyondViewport": false,
                "optimizeForSpeed": true
            }),
        );
        let settled = STANDARD.decode(settled["data"].as_str().unwrap()).unwrap();
        assert_eq!(jpeg_dimensions(&settled), Some((1280, 960)));

        execute("Page.stopScreencast", json!({}));
        engine.unsubscribe(subscription);
    }
}
