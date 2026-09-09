pub mod claude;
pub mod codex;
pub mod dsh;
pub mod grok;
pub mod images;
pub mod kimi;
pub mod models;
pub mod pi_family;
pub mod pi_family_auth;
pub mod resolve;

pub(crate) use resolve::command_for_binary;

use crate::event_sink;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::Mutex as TokioMutex;

/// Windows pops a visible console window for every console-subsystem child a
/// GUI process spawns (engine CLIs are node/.cmd shims, so every probe and
/// run flashes one). Suppress it — the child's stdio is piped, the console
/// would be useless anyway.
#[cfg(windows)]
pub(crate) fn hide_console(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

pub struct SendRequest {
    pub session_id: Option<String>,
    pub workspace: PathBuf,
    pub prompt: String,
    pub images: Vec<String>,
    pub model: Option<String>,
    /// Reasoning effort ("low" | "medium" | "high" | "xhigh" | "max"); engines without an
    /// effort knob ignore it, engines with a narrower knob clamp.
    pub effort: Option<String>,
    /// OMP OpenAI service tier override, independent of reasoning effort.
    pub service_tier: Option<String>,
    /// Permission mode ("auto" | "manual" | "plan" | "bypass"); each engine
    /// resolves it against the modes it can actually honor at spawn (see
    /// `Engine::resolve_permission`).
    pub permission: Option<String>,
}

pub struct BuiltCommand {
    pub command: Command,
    /// Written to stdin after spawn; stdin is then closed.
    pub stdin_payload: Option<String>,
    /// Temp files to remove once the process exits.
    pub cleanup_files: Vec<PathBuf>,
    /// Session id assigned before spawn (grok `-s <uuid>`).
    pub preassigned_session_id: Option<String>,
}

pub enum EngineEvent {
    /// Streaming text delta (append).
    Delta(String),
    /// Reasoning/thinking delta (append).
    Thinking(String),
    /// A completed message block (role, text). `path` carries the target
    /// file of a tool call (read/edit/write/...) so the UI can render a
    /// file chip; None for everything else.
    Message {
        role: String,
        text: String,
        path: Option<String>,
        /// Todo-list snapshot/patch from a todo tool call (claude TodoWrite,
        /// omp todo op); feeds the run-status strip's task pill.
        todos: Option<TodosPayload>,
    },
    /// Native session id became known.
    SessionId(String),
    /// Token usage snapshot from the engine.
    Usage(Value),
    /// Engine-reported error.
    Error(String),
    /// Non-terminal engine notice (e.g. an upstream 429 the CLI is
    /// retrying): surfaced to the UI, but the turn is still running.
    Warn(String),
    /// Turn finished successfully.
    Done {
        session_id: Option<String>,
        usage: Option<Value>,
    },
}

/// One todo entry carried to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

/// `replace: true` is a full snapshot of the todo list; `false` is a patch
/// the frontend applies by matching on `content`.
#[derive(Debug, Clone, Serialize)]
pub struct TodosPayload {
    pub items: Vec<TodoItem>,
    pub replace: bool,
}

/// First path-like argument of a tool call (`read`/`edit`/`write` use
/// `path`, claude's tools use `file_path`). Returns None for tools whose
/// args carry no file target (e.g. bash `command`). Glob patterns are kept
/// as-is; the UI decides whether the string is chip-worthy.
pub(crate) fn tool_path_arg(args: &Value) -> Option<String> {
    ["path", "file_path", "filePath"]
        .iter()
        .filter_map(|key| args.get(key).and_then(Value::as_str))
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Parse a tool call's args into a todo-list payload. Two shapes: claude's
/// TodoWrite (`todos` array, a full snapshot) and the omp harness todo
/// protocol (`op` + task/list, mostly patches). None when the args carry
/// no todo data.
pub(crate) fn parse_todo_args(args: &Value) -> Option<TodosPayload> {
    let pending_item = |content: &str| TodoItem {
        content: content.to_string(),
        status: "pending".to_string(),
    };
    // `list` phases, each with an `items` string array, flattened.
    let phase_items = |args: &Value| -> Vec<TodoItem> {
        args.get("list")
            .and_then(Value::as_array)
            .map(|phases| {
                phases
                    .iter()
                    .filter_map(|phase| phase.get("items").and_then(Value::as_array))
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(pending_item)
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(todos) = args.get("todos").and_then(Value::as_array) {
        let items = todos
            .iter()
            .filter_map(|entry| {
                let content = ["content", "text", "title", "task"]
                    .iter()
                    .filter_map(|key| entry.get(key).and_then(Value::as_str))
                    .map(|s| s.trim())
                    .find(|s| !s.is_empty())?;
                let status = match entry.get("status").and_then(Value::as_str).unwrap_or("") {
                    "in_progress" | "running" | "active" => "active",
                    "completed" | "complete" | "done" => "complete",
                    "blocked" => "blocked",
                    _ => "pending",
                };
                Some(TodoItem {
                    content: content.to_string(),
                    status: status.to_string(),
                })
            })
            .collect();
        return Some(TodosPayload {
            items,
            replace: true,
        });
    }
    let op = args.get("op").and_then(Value::as_str)?;
    match op {
        "init" => Some(TodosPayload {
            items: phase_items(args),
            replace: true,
        }),
        "append" => {
            let items = match args.get("items").and_then(Value::as_array) {
                Some(items) => items.iter().filter_map(Value::as_str).map(pending_item).collect(),
                None => phase_items(args),
            };
            Some(TodosPayload {
                items,
                replace: false,
            })
        }
        "start" | "done" | "block" | "unblock" | "drop" => {
            let task = args.get("task").and_then(Value::as_str)?;
            let status = match op {
                "start" => "active",
                "done" => "complete",
                "block" => "blocked",
                "unblock" => "pending",
                _ => "dropped",
            };
            Some(TodosPayload {
                items: vec![TodoItem {
                    content: task.to_string(),
                    status: status.to_string(),
                }],
                replace: false,
            })
        }
        "rm" | "clear" => Some(TodosPayload {
            items: Vec::new(),
            replace: true,
        }),
        _ => None,
    }
}

pub trait Engine: Send + Sync {
    fn id(&self) -> &'static str;
    fn build_command(&self, req: &SendRequest, bin: &str) -> Result<BuiltCommand, String>;
    /// Parse one NDJSON stdout line into zero or more events.
    fn parse_line(&self, line: &str, out: &mut Vec<EngineEvent>);
    /// Whether this engine accepts image attachments.
    fn supports_images(&self) -> bool;
    /// Permission modes this engine can honor at spawn ("auto" | "manual" |
    /// "plan" | "bypass"). These are one-shot headless launches that cannot
    /// ask mid-turn, so most engines support only a subset; the UI greys out
    /// the rest rather than promising a mode the CLI would silently ignore.
    fn supported_permissions(&self) -> &'static [&'static str] {
        &["auto"]
    }
    /// Effective mode for one send: the requested mode when this engine
    /// supports it, otherwise the engine's first supported mode.
    fn resolve_permission(&self, requested: Option<&str>) -> &'static str {
        let supported = self.supported_permissions();
        requested
            .and_then(|mode| supported.iter().copied().find(|m| *m == mode))
            .unwrap_or(supported[0])
    }
}

pub fn engine_by_id(id: &str) -> Option<Box<dyn Engine>> {
    match id {
        "claude" => Some(Box::new(claude::ClaudeEngine)),
        "kimi" => Some(Box::new(kimi::KimiEngine)),
        "grok" => Some(Box::new(grok::GrokEngine)),
        "codex" => Some(Box::new(codex::CodexEngine)),
        "pi" => Some(Box::new(pi_family::pi())),
        "omp" => Some(Box::new(pi_family::omp())),
        "dsh" => Some(Box::new(dsh::DshEngine)),
        _ => None,
    }
}

/// Engine home dir: `$ENV_KEY` (with `~` expansion, as the CLIs resolve it)
/// when set and non-empty, else `~/<default_dir>`.
pub(crate) fn engine_home(env_key: Option<&str>, default_dir: &str) -> PathBuf {
    if let Some(key) = env_key {
        if let Some(value) = std::env::var_os(key).filter(|v| !v.is_empty()) {
            let text = value.to_string_lossy();
            if let Ok(expanded) = crate::open_app::expand_user_path(&text) {
                return expanded;
            }
            return PathBuf::from(value);
        }
    }
    dirs::home_dir().unwrap_or_default().join(default_dir)
}

/// A leading '-' would parse as a flag (pi also treats '@' as a file
/// reference): prefix a space so the prompt stays positional text.
pub(crate) fn safe_prompt_arg(prompt: &str) -> String {
    if prompt.starts_with('-') || prompt.starts_with('@') {
        format!(" {prompt}")
    } else {
        prompt.to_string()
    }
}

/// Push a `SessionId` event from a JSON string field; blank values are ignored.
/// Engines disagree on the key (`session_id` / `thread_id` / `id`).
pub(crate) fn push_session_id(value: &Value, key: &str, out: &mut Vec<EngineEvent>) {
    if let Some(id) = value.get(key).and_then(Value::as_str) {
        if !id.trim().is_empty() {
            out.push(EngineEvent::SessionId(id.trim().to_string()));
        }
    }
}

// ==================== Process registry ====================

/// Live engine child processes keyed by session key (native session id once
/// known, otherwise the run id). Drop kills everything synchronously.
pub struct ChildEntry {
    pub child: Arc<TokioMutex<Child>>,
    pub pid: u32,
    /// The run id this entry started under; after a rekey the map key is the
    /// native session id, but the frontend may still cancel by run id.
    pub run_id: String,
    /// Set by `kill()`: a user-initiated stop is not an error — at EOF the
    /// runner commits the partial turn as done instead of pushing a bogus
    /// "exited with status …" error.
    pub killed: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
pub struct ProcessRegistry(pub Mutex<HashMap<String, ChildEntry>>);

/// Two concurrent runs of one session must never evict each other's entries:
/// an evicted child leaks (no key routes an interrupt to it).
impl ProcessRegistry {
    fn insert(&self, key: String, entry: ChildEntry) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(key, entry);
        }
    }

    fn len(&self) -> usize {
        self.0.lock().map(|map| map.len()).unwrap_or(0)
    }

    /// Move an entry to the native-session key once known. A colliding target
    /// key belongs to another live run — keep both instead of overwriting.
    fn rekey(&self, from: &str, to: String) {
        if from == to {
            return;
        }
        if let Ok(mut map) = self.0.lock() {
            if map.contains_key(&to) {
                return;
            }
            if let Some(entry) = map.remove(from) {
                map.insert(to, entry);
            }
        }
    }

    /// Remove only if the entry is still the same child (pid match): a run
    /// that lost its session key to nothing must not evict another run's
    /// entry that now lives under that key.
    fn remove_if_pid(&self, key: &str, pid: u32) {
        if let Ok(mut map) = self.0.lock() {
            if map.get(key).map(|entry| entry.pid) == Some(pid) {
                map.remove(key);
            }
        }
    }

    /// Ask the OS to terminate the whole CLI process tree and wait until the
    /// request is accepted. Returning `Ok(true)` means the OS confirmed the
    /// termination command; `Ok(false)` means no live run matched the key.
    pub async fn kill(&self, key: &str) -> Result<bool, String> {
        let entry = match self.0.lock() {
            Ok(map) => map
                .get(key)
                .map(|e| (e.pid, Arc::clone(&e.child), Arc::clone(&e.killed))),
            Err(_) => None,
        };
        // Fallback: the frontend may cancel by run id after the entry was
        // rekeyed to the native session id.
        let entry = entry.or_else(|| {
            self.0.lock().ok().and_then(|map| {
                map.values()
                    .find(|e| e.run_id == key)
                    .map(|e| (e.pid, Arc::clone(&e.child), Arc::clone(&e.killed)))
            })
        });
        let Some((pid, child, killed)) = entry else {
            return Ok(false);
        };
        if let Ok(mut guard) = child.try_lock() {
            // Pid-reuse guard: once a child was reaped, its OS pid may have
            // been assigned to another process. A completed run is already
            // stopped, so never pass that pid to taskkill.
            if matches!(guard.try_wait(), Ok(Some(_))) {
                return Ok(true);
            }
        }
        killed.store(true, std::sync::atomic::Ordering::SeqCst);
        if terminate_process_tree(pid).await.is_ok() {
            return Ok(true);
        }

        // Do not report a successful stop when Windows rejects `taskkill`.
        // Best-effort kill the direct child, then let the frontend show an
        // actionable error because grandchildren may still be alive.
        killed.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = child.try_lock() {
            let _ = guard.start_kill();
        }
        Err("failed to terminate the CLI process tree".to_string())
    }

    pub fn kill_all(&self) {
        // Blocking lock on the teardown path: skipping children because the
        // lock was briefly contended would leak engine processes.
        let entries: Vec<ChildEntry> = match self.0.lock() {
            Ok(mut map) => map.drain().map(|(_, e)| e).collect(),
            Err(poisoned) => poisoned.into_inner().drain().map(|(_, e)| e).collect(),
        };
        for entry in entries {
            kill_process_group(entry.pid);
            if let Ok(mut guard) = entry.child.try_lock() {
                let _ = guard.start_kill();
            }
        }
    }
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        // &mut self makes locking unnecessary; poisoning must not skip the
        // kill sweep either (a panicked run leaves live children).
        let map = self.0.get_mut().unwrap_or_else(|e| e.into_inner());
        for (_, entry) in map.drain() {
            kill_process_group(entry.pid);
            if let Ok(mut guard) = entry.child.try_lock() {
                let _ = guard.start_kill();
            }
        }
    }
}
/// SIGKILL the child's whole process group (spawn used `process_group(0)`,
/// so pgid == pid). Grandchildren holding the stdout pipe die too, which is
/// what lets the reader task observe EOF and drain the registry.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    // SAFETY: kill with a negated pgid signals the group; no memory touched.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

/// Windows has no process groups; npm CLIs spawn as `cmd /c x.cmd`, so the
/// real CLI is a grandchild. Killing only the direct child (start_kill)
/// orphans node — the turn keeps streaming and burning API calls, and its
/// inherited stdout pipe never reaches EOF. `taskkill /T /F` takes the
/// whole tree down. Fire-and-forget: the callers' start_kill still handles
/// the direct child synchronously.
#[cfg(not(unix))]
pub(crate) fn kill_process_group(pid: u32) {
    let mut command = std::process::Command::new("taskkill");
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command.spawn();
}

/// Confirm a user-requested process-tree termination. Shutdown cleanup uses
/// the non-blocking helper above; the interactive Stop action needs a result
/// so the UI never claims success when `taskkill` was rejected.
#[cfg(unix)]
async fn terminate_process_tree(pid: u32) -> Result<(), String> {
    // SAFETY: signal only the process group created for this child at spawn.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // A concurrent clean exit is equivalent to a successful stop.
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("failed to terminate the CLI process tree: {error}"))
}

#[cfg(not(unix))]
async fn terminate_process_tree(pid: u32) -> Result<(), String> {
    let mut command = Command::new("taskkill");
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command
            .as_std_mut()
            .creation_flags(CREATE_NO_WINDOW);
    }
    let status = command
        .status()
        .await
        .map_err(|error| format!("failed to run taskkill: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("taskkill exited with status {status}"))
    }
}

// ==================== stderr redaction ====================

/// Engine stderr can echo the channel credentials from the CLI's own config files; redact credential
/// shapes before the tail is shown to the user in an error banner.
fn redact_secrets(text: &str) -> String {
    use std::sync::LazyLock;
    static PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
        [
            r"(?i)sk-[A-Za-z0-9_-]+",
            r"(?i)bearer\s+\S+",
            r"(?i)api[_-]?key\s*[=:]\s*\S+",
            r"(?i)token\s*[=:]\s*\S+",
        ]
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
    });
    let mut out = text.to_string();
    for pattern in PATTERNS.iter() {
        out = pattern.replace_all(&out, "***").into_owned();
    }
    out
}

// ==================== Commands ====================

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub run_id: String,
    pub session_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    pub id: String,
    pub available: bool,
    /// False when the user disabled this CLI in settings; the UI hides it
    /// from pickers and history lists rather than erroring on launch.
    pub enabled: bool,
    pub supports_images: bool,
    /// Permission modes the engine honors at spawn; drives the composer
    /// picker's disabled options.
    pub permissions: Vec<String>,
}

fn engine_bin(settings: &crate::settings::AppSettings, engine_id: &str) -> String {
    if let Some(custom) = settings.bin_override(engine_id) {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            // Defense in depth: settings write validates too, but the file
            // may have been hand-edited since.
            match crate::settings::validate_bin_override(trimmed) {
                Ok(path) => return resolve::resolve_launchable_cli_binary(&path.to_string_lossy()),
                Err(reason) => {
                    eprintln!("[engine] ignoring invalid {engine_id} bin override: {reason}");
                }
            }
        }
    }
    resolve::resolve_launchable_cli_binary(engine_id)
}

#[tauri::command]
pub fn list_engines() -> Vec<EngineInfo> {
    let settings = crate::settings::read_settings().unwrap_or_default();
    let config = crate::config::read_config().unwrap_or_default();
    crate::config::ENGINES
        .iter()
        .map(|id| {
            let engine = engine_by_id(id).expect("known engine");
            let available = match settings.bin_override(id) {
                Some(custom) if !custom.trim().is_empty() => {
                    crate::settings::validate_bin_override(custom).is_ok()
                }
                _ => resolve::find_cli_binary(id, None).is_some(),
            };
            EngineInfo {
                id: id.to_string(),
                available,
                enabled: config.section(id).and_then(|s| s.current.as_deref())
                    != Some(crate::config::DISABLED_PROVIDER_ID),
                supports_images: engine.supports_images(),
                permissions: engine
                    .supported_permissions()
                    .iter()
                    .map(|m| m.to_string())
                    .collect(),
            }
        })
        .collect()
}

/// Concurrent engine runs; past this the machine thrashes and the registry
/// fan-out makes interrupts unreliable anyway.
const MAX_CONCURRENT_RUNS: usize = 16;

/// Resolved launch parameters for one send: request, binary, built command.
struct Launch {
    req: SendRequest,
    bin: String,
    built: BuiltCommand,
    engine_impl: Box<dyn Engine>,
}

fn prepare_launch(
    engine: &str,
    workspace_path: &str,
    session_id: Option<String>,
    prompt: String,
    image_paths: Option<Vec<String>>,
    model: Option<String>,
    effort: Option<String>,
    permission: Option<String>,
) -> Result<Launch, String> {
    let engine_impl = engine_by_id(engine).ok_or_else(|| format!("unknown engine: {engine}"))?;
    // Channels live in each CLI's native config file (provider_files); the
    // only launch-time gate left is the 停用 pseudo-provider.
    crate::config::ensure_engine_enabled(engine)?;
    let settings = crate::settings::read_settings().unwrap_or_default();
    let model = model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| settings.default_models.get(engine).cloned())
        .filter(|m| !m.trim().is_empty());
    let effort = effort
        .filter(|e| !e.trim().is_empty())
        .or_else(|| settings.default_efforts.get(engine).cloned())
        .filter(|e| !e.trim().is_empty());
    let req = SendRequest {
        session_id: session_id.filter(|s| !s.trim().is_empty()),
        workspace: PathBuf::from(workspace_path),
        prompt,
        images: image_paths.unwrap_or_default(),
        model,
        effort,
        service_tier: if engine == "omp" {
            settings.omp_openai_service_tier.clone()
        } else {
            None
        },
        permission: permission.filter(|p| !p.trim().is_empty()),
    };
    let bin = engine_bin(&settings, engine);
    let built = engine_impl.build_command(&req, &bin)?;
    Ok(Launch {
        req,
        bin,
        built,
        engine_impl,
    })
}

/// Stdin payload writer: engines consuming stream-json stdin get the payload
/// then EOF (drop closes the pipe).
fn spawn_stdin_writer(child: &mut Child, payload: Option<String>) {
    let Some(payload) = payload else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            // drop closes stdin -> EOF
        });
    }
}

/// Stderr capture ring: keeps the last 4KB for the error banner.
fn spawn_stderr_capture(stderr: ChildStderr) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let target = Arc::clone(&buf);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut guard = match target.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    guard.push_str(&line);
                    if guard.len() > 4096 {
                        let keep = guard.len() - 4096;
                        guard.drain(..keep);
                    }
                }
            }
        }
    });
    buf
}

/// Mutable per-run streaming state shared by the reader loop's dispatch.
struct TurnState {
    seq: u64,
    native_session_id: Option<String>,
    saw_done: bool,
    saw_error: bool,
    saw_any_output: bool,
}

impl TurnState {
    fn new(preassigned: Option<String>) -> Self {
        Self {
            seq: 0,
            native_session_id: preassigned,
            saw_done: false,
            saw_error: false,
            saw_any_output: false,
        }
    }

    fn push(
        &mut self,
        sink: &Arc<event_sink::EventSink>,
        run_id: &str,
        engine_id: &str,
        kind: &str,
        data: Value,
    ) {
        self.seq += 1;
        sink.push(serde_json::json!({
            "runId": run_id,
            "sessionId": self.native_session_id,
            "engine": engine_id,
            "seq": self.seq,
            "kind": kind,
            "data": data,
        }));
    }
}

/// Everything the stdout reader task needs (moved in at spawn).
struct RunContext {
    sink: Arc<event_sink::EventSink>,
    registry: Arc<ProcessRegistry>,
    engine_impl: Box<dyn Engine>,
    engine_id: String,
    run_id: String,
    pid: u32,
    /// Session id fixed before spawn (grok `-s`); seeds TurnState.
    preassigned_session_id: Option<String>,
    child: Arc<TokioMutex<Child>>,
    killed: Arc<std::sync::atomic::AtomicBool>,
    cleanup_files: Vec<PathBuf>,
    stderr_buf: Arc<Mutex<String>>,
}

impl RunContext {
    /// Adopt a native session id: rekey the registry entry (no overwrite) and
    /// remember it for subsequent event payloads.
    fn adopt_session_id(&self, state: &mut TurnState, id: &str, announce: bool) {
        if state.native_session_id.as_deref() == Some(id) {
            return;
        }
        state.native_session_id = Some(id.to_string());
        self.registry.rekey(&self.run_id, id.to_string());
        if announce {
            state.push(
                &self.sink,
                &self.run_id,
                &self.engine_id,
                "session",
                Value::String(id.to_string()),
            );
        }
    }

    fn dispatch_event(&self, state: &mut TurnState, event: EngineEvent) {
        match event {
            EngineEvent::Delta(text) => state.push(
                &self.sink,
                &self.run_id,
                &self.engine_id,
                "delta",
                Value::String(text),
            ),
            EngineEvent::Thinking(text) => state.push(
                &self.sink,
                &self.run_id,
                &self.engine_id,
                "thinking",
                Value::String(text),
            ),
            EngineEvent::Message {
                role,
                text,
                path,
                todos,
            } => {
                let mut payload = serde_json::json!({ "role": role, "text": text });
                if let Some(path) = path {
                    payload["path"] = Value::String(path);
                }
                if let Some(todos) = todos {
                    if let Ok(value) = serde_json::to_value(todos) {
                        payload["todos"] = value;
                    }
                }
                state.push(
                    &self.sink,
                    &self.run_id,
                    &self.engine_id,
                    "message",
                    payload,
                )
            }
            EngineEvent::SessionId(id) => self.adopt_session_id(state, &id, true),
            EngineEvent::Usage(usage) => {
                state.push(&self.sink, &self.run_id, &self.engine_id, "usage", usage)
            }
            EngineEvent::Error(error) => {
                state.saw_error = true;
                state.push(
                    &self.sink,
                    &self.run_id,
                    &self.engine_id,
                    "error",
                    Value::String(error),
                );
            }
            EngineEvent::Warn(error) => {
                // Not terminal: no saw_error — EOF settle still decides the
                // turn's fate if the CLI gives up after this notice.
                state.push(
                    &self.sink,
                    &self.run_id,
                    &self.engine_id,
                    "warn",
                    Value::String(error),
                );
            }
            EngineEvent::Done { session_id, usage } => {
                state.saw_done = true;
                if let Some(id) = session_id {
                    self.adopt_session_id(state, &id, false);
                }
                state.push(
                    &self.sink,
                    &self.run_id,
                    &self.engine_id,
                    "done",
                    serde_json::json!({ "usage": usage }),
                );
            }
        }
    }
}

/// Read NDJSON stdout until EOF, dispatch events, then settle the turn:
/// registry cleanup, temp-file cleanup, and the terminal done/error event.
async fn run_reader(stdout: ChildStdout, ctx: RunContext) {
    let mut state = TurnState::new(ctx.preassigned_session_id.clone());
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        // A child can leave already-written bytes in stdout after Stop. Do
        // not turn those stale bytes into frontend events while taskkill is
        // still tearing down the Windows process tree.
        if ctx.killed.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        state.saw_any_output = true;
        let mut events = Vec::new();
        ctx.engine_impl.parse_line(trimmed, &mut events);
        for event in events {
            if ctx.killed.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            ctx.dispatch_event(&mut state, event);
        }
    }

    // Wait for exit
    let status = {
        let mut guard = ctx.child.lock().await;
        guard.wait().await.ok()
    };
    for path in &ctx.cleanup_files {
        let _ = std::fs::remove_file(path);
    }
    if let Some(key) = state.native_session_id.clone() {
        ctx.registry.remove_if_pid(&key, ctx.pid);
    }
    ctx.registry.remove_if_pid(&ctx.run_id, ctx.pid);

    if !state.saw_done && !state.saw_error {
        let stderr_tail = ctx
            .stderr_buf
            .lock()
            .map(|g| g.trim().to_string())
            .unwrap_or_default();
        let failed = status.map(|s| !s.success()).unwrap_or(true);
        let killed = ctx.killed.load(std::sync::atomic::Ordering::SeqCst);
        if killed {
            // User-initiated stop: commit whatever streamed so far as a
            // normal turn end — a SIGKILL'd child is not a failure.
            state.push(
                &ctx.sink,
                &ctx.run_id,
                &ctx.engine_id,
                "done",
                serde_json::json!({ "usage": null }),
            );
        } else if failed || !state.saw_any_output {
            let mut message = format!(
                "{} exited with status {}",
                ctx.engine_id,
                status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            if !stderr_tail.is_empty() {
                message.push_str(&format!(": {}", redact_secrets(&stderr_tail)));
            }
            state.push(
                &ctx.sink,
                &ctx.run_id,
                &ctx.engine_id,
                "error",
                Value::String(message),
            );
        } else {
            // Clean EOF without an explicit done line (kimi).
            state.push(
                &ctx.sink,
                &ctx.run_id,
                &ctx.engine_id,
                "done",
                serde_json::json!({ "usage": null }),
            );
        }
    }
    ctx.sink.flush();
}

#[tauri::command]
pub async fn send_message(
    state: tauri::State<'_, crate::AppState>,
    engine: String,
    workspace_path: String,
    session_id: Option<String>,
    prompt: String,
    image_paths: Option<Vec<String>>,
    model: Option<String>,
    effort: Option<String>,
    permission: Option<String>,
) -> Result<SendResult, String> {
    if state.processes.len() >= MAX_CONCURRENT_RUNS {
        return Err(format!(
            "too many concurrent runs ({MAX_CONCURRENT_RUNS}); wait for one to finish"
        ));
    }
    let launch = prepare_launch(
        &engine,
        &workspace_path,
        session_id,
        prompt,
        image_paths,
        model,
        effort,
        permission,
    )?;

    let mut command = launch.built.command;
    command
        .stdin(if launch.built.stdin_payload.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(&launch.req.workspace);
    // Own process group so interrupt can kill the whole tree (grandchildren
    // inherit the stdout pipe and would otherwise block EOF forever).
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    hide_console(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // Never strand the staging files build_command wrote (grok).
            for path in &launch.built.cleanup_files {
                let _ = std::fs::remove_file(path);
            }
            return Err(format!("failed to spawn {}: {error}", launch.bin));
        }
    };

    spawn_stdin_writer(&mut child, launch.built.stdin_payload);

    let run_id = uuid::Uuid::new_v4().to_string();
    let pid = child.id().unwrap_or(0);
    // Detach both pipes while we still own the child outright. A missing pipe
    // after spawn is fatal: kill the child so it cannot run unobserved and
    // unregistered.
    let (stdout, stderr) = {
        let pipes = child.stdout.take().zip(child.stderr.take());
        match pipes {
            Some(pair) => pair,
            None => {
                let _ = child.start_kill();
                for path in &launch.built.cleanup_files {
                    let _ = std::fs::remove_file(path);
                }
                return Err("missing stdout/stderr pipe after spawn".to_string());
            }
        }
    };
    let child = Arc::new(TokioMutex::new(child));
    let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.processes.insert(
        run_id.clone(),
        ChildEntry {
            child: Arc::clone(&child),
            pid,
            run_id: run_id.clone(),
            killed: Arc::clone(&killed),
        },
    );

    let stderr_buf = spawn_stderr_capture(stderr);
    let ctx = RunContext {
        sink: Arc::clone(&state.sink),
        registry: Arc::clone(&state.processes),
        engine_impl: launch.engine_impl,
        engine_id: engine.clone(),
        run_id: run_id.clone(),
        pid,
        preassigned_session_id: launch.built.preassigned_session_id.clone(),
        child,
        killed,
        cleanup_files: launch.built.cleanup_files,
        stderr_buf,
    };
    tokio::spawn(run_reader(stdout, ctx));

    Ok(SendResult {
        run_id,
        session_id: launch.built.preassigned_session_id,
    })
}

#[tauri::command]
pub async fn interrupt_session(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
) -> Result<bool, String> {
    state.processes.kill(&session_id).await
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    fn req(permission: Option<&str>) -> SendRequest {
        SendRequest {
            session_id: None,
            workspace: PathBuf::from("/tmp"),
            prompt: "hi".to_string(),
            images: Vec::new(),
            model: None,
            effort: None,
            service_tier: None,
            permission: permission.map(str::to_string),
        }
    }

    fn argv(engine: &dyn Engine, req: &SendRequest) -> Vec<String> {
        let built = engine.build_command(req, "fake-bin").unwrap();
        built
            .command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn omp_fast_tier_is_explicit_and_independent_of_effort() {
        let mut request = req(None);
        request.model = Some("openai-codex/gpt-5.4".into());
        request.effort = Some("high".into());
        for tier in [None, Some("priority"), Some("default")] {
            request.service_tier = tier.map(str::to_string);
            let args = argv(&pi_family::omp(), &request);
            let actual = args
                .iter()
                .position(|a| a == "--service-tier")
                .map(|i| args[i + 1].as_str());
            assert_eq!(actual, tier);
            assert!(args.windows(2).any(|a| a == ["--thinking", "high"]));
        }
    }

    #[test]
    fn omp_fast_tier_does_not_leak_to_other_models_or_pi() {
        let mut request = req(None);
        request.service_tier = Some("priority".into());
        for model in [
            None,
            Some("anthropic/claude"),
            Some("google/gemini"),
            Some("gpt-5.4"),
            Some("openai/"),
            Some("openai/gpt-5.4"),
            Some("openai-codex/"),
            Some("custom/gpt-5.4"),
        ] {
            request.model = model.map(str::to_string);
            assert!(!argv(&pi_family::omp(), &request)
                .iter()
                .any(|a| a == "--service-tier"));
        }
        request.model = Some("openai-codex/gpt-5.4".into());
        assert!(!argv(&pi_family::pi(), &request)
            .iter()
            .any(|a| a == "--service-tier"));
        assert!(argv(&pi_family::omp(), &request)
            .iter()
            .any(|a| a == "--service-tier"));
        request.service_tier = Some("invalid".into());
        assert!(pi_family::omp()
            .build_command(&request, "fake-bin")
            .is_err());
    }

    #[test]
    fn omp_tier_settings_are_backward_compatible_and_roundtrip() {
        let mut settings: crate::settings::AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.omp_openai_service_tier, None);
        for tier in [Some("priority"), Some("default"), None] {
            settings.omp_openai_service_tier = tier.map(str::to_string);
            let encoded = serde_json::to_string(&settings).unwrap();
            let decoded: crate::settings::AppSettings = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.omp_openai_service_tier.as_deref(), tier);
        }
    }

    #[test]
    fn unsupported_mode_falls_back_to_first_supported() {
        let codex = codex::CodexEngine;
        assert_eq!(codex.resolve_permission(Some("plan")), "auto");
        assert_eq!(codex.resolve_permission(Some("manual")), "manual");
        assert_eq!(codex.resolve_permission(None), "auto");
        let grok = grok::GrokEngine;
        assert_eq!(grok.resolve_permission(Some("auto")), "bypass");
    }

    #[test]
    fn claude_maps_modes_to_permission_flags() {
        let e = claude::ClaudeEngine;
        let auto = argv(&e, &req(Some("auto")));
        assert!(auto.contains(&"--permission-mode".to_string()));
        assert!(auto.contains(&"acceptEdits".to_string()));
        assert!(!auto.contains(&"--dangerously-skip-permissions".to_string()));

        let manual = argv(&e, &req(Some("manual")));
        assert!(manual.contains(&"default".to_string()));

        let plan = argv(&e, &req(Some("plan")));
        assert!(plan.contains(&"plan".to_string()));

        let bypass = argv(&e, &req(Some("bypass")));
        assert!(bypass.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!bypass.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn codex_maps_modes_to_sandbox_flags() {
        let e = codex::CodexEngine;
        let auto = argv(&e, &req(Some("auto")));
        assert!(auto.contains(&"sandbox_mode=\"workspace-write\"".to_string()));
        assert!(!auto.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));

        let manual = argv(&e, &req(Some("manual")));
        assert!(manual.contains(&"sandbox_mode=\"read-only\"".to_string()));

        let bypass = argv(&e, &req(Some("bypass")));
        assert!(bypass.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn codex_resume_avoids_unsupported_sandbox_flag() {
        // `codex exec resume` rejects --sandbox (clap exit 2); the sandbox must
        // travel via -c sandbox_mode on both fresh and resumed sessions.
        let e = codex::CodexEngine;
        let mut resume = req(Some("manual"));
        resume.session_id = Some("00000000-0000-0000-0000-000000000000".to_string());
        let args = argv(&e, &resume);
        assert!(args.contains(&"resume".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
        assert!(args.contains(&"sandbox_mode=\"read-only\"".to_string()));

        let fresh = argv(&e, &req(Some("auto")));
        assert!(!fresh.contains(&"--sandbox".to_string()));
        assert!(fresh.contains(&"sandbox_mode=\"workspace-write\"".to_string()));
    }

    #[test]
    fn kimi_maps_plan_and_bypass() {
        let e = kimi::KimiEngine;
        let auto = argv(&e, &req(Some("auto")));
        assert!(!auto.contains(&"--yolo".to_string()));
        assert!(!auto.contains(&"--plan".to_string()));

        let plan = argv(&e, &req(Some("plan")));
        assert!(plan.contains(&"--plan".to_string()));

        let bypass = argv(&e, &req(Some("bypass")));
        assert!(bypass.contains(&"--yolo".to_string()));

        // Manual is unsupported: falls back to auto (no flags).
        let manual = argv(&e, &req(Some("manual")));
        assert!(!manual.contains(&"--yolo".to_string()));
        assert!(!manual.contains(&"--plan".to_string()));
    }

    #[test]
    fn grok_always_approves_regardless_of_request() {
        let e = grok::GrokEngine;
        for mode in [
            Some("auto"),
            Some("manual"),
            Some("plan"),
            Some("bypass"),
            None,
        ] {
            assert!(argv(&e, &req(mode)).contains(&"--always-approve".to_string()));
        }
    }
}
