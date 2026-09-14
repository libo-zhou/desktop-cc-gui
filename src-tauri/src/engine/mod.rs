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
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    /// User-granted extra directories (db `granted_roots`); claude launches
    /// pass them as `--add-dir` so reads outside the workspace stop hitting
    /// headless permission denials. Engines without an equivalent flag
    /// ignore them.
    pub additional_dirs: Vec<String>,
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

#[derive(Debug)]
pub enum EngineEvent {
    /// Streaming text delta (append).
    Delta(String),
    /// Reasoning/thinking delta (append).
    Thinking(String),
    /// A completed message block (role, text). `path` carries the target
    /// file of a tool call (read/edit/write/...) so the UI can render a
    /// file chip; None for everything else. `args` is the tool-call payload
    /// (pretty-printed in the timeline). `patch` updates the oldest
    /// still-incomplete tool row of the same name (claude streams args
    /// after the name-only start).
    Message {
        role: String,
        text: String,
        path: Option<String>,
        /// Todo-list snapshot/patch from a todo tool call (claude TodoWrite,
        /// omp todo op); feeds the run-status strip's task pill.
        todos: Option<TodosPayload>,
        args: Option<Value>,
        result: Option<Value>,
        patch: bool,
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
    /// A tool call was denied by the CLI's permission system (headless mode
    /// cannot prompt). `path` is the denied absolute path when the denial
    /// text or tool input carries one — the UI offers a directory grant for
    /// it; `tool` is the denied tool name when known.
    PermissionDenied {
        tool: Option<String>,
        path: Option<String>,
        message: String,
    },
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

/// Drop empty / null payloads so the UI does not render a blank args panel.
/// JSON-encoded strings (OpenAI-style `function.arguments`) are parsed first.
pub(crate) fn parse_tool_args_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Object(map) if map.is_empty() => None,
        Value::Array(items) if items.is_empty() => None,
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(parsed) => parse_tool_args_value(&parsed).or_else(|| Some(Value::String(trimmed.to_string()))),
                Err(_) => Some(Value::String(trimmed.to_string())),
            }
        }
        other => Some(other.clone()),
    }
}

/// Tool-call start: name plus parsed args (path / todos derived from args).
pub(crate) fn tool_call_message(name: impl Into<String>, args: Option<&Value>) -> EngineEvent {
    let args = args.and_then(parse_tool_args_value);
    EngineEvent::Message {
        role: "tool".to_string(),
        text: name.into(),
        path: args.as_ref().and_then(tool_path_arg),
        todos: args.as_ref().and_then(parse_todo_args),
        args,
        result: None,
        patch: false,
    }
}

/// Same as [`tool_call_message`] but patches the matching in-flight tool row.
pub(crate) fn tool_call_patch(name: impl Into<String>, args: Option<&Value>) -> EngineEvent {
    match tool_call_message(name, args) {
        EngineEvent::Message {
            role,
            text,
            path,
            todos,
            args,
            ..
        } => EngineEvent::Message {
            role,
            text,
            path,
            todos,
            args,
            result: None,
            patch: true,
        },
        other => other,
    }
}

/// Patches execution result onto the matching in-flight tool row.
pub(crate) fn tool_result_patch(name: impl Into<String>, result: Option<&Value>) -> EngineEvent {
    EngineEvent::Message {
        role: "tool".to_string(),
        text: name.into(),
        path: None,
        todos: None,
        args: None,
        result: result.cloned(),
        patch: true,
    }
}

/// Assistant snapshot with no tool metadata.
pub(crate) fn assistant_message(text: String) -> EngineEvent {
    EngineEvent::Message {
        role: "assistant".to_string(),
        text,
        path: None,
        todos: None,
        args: None,
        result: None,
        patch: false,
    }
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
        "claude" => Some(Box::new(claude::ClaudeEngine::new())),
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
    fallback_home().join(default_dir)
}

/// Home dir for the default engine path. Production uses `dirs` (Known
/// Folder API on Windows); tests steer the fallback through HOME /
/// USERPROFILE env instead, because `dirs` ignores env on Windows and would
/// scan the real profile.
fn fallback_home() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home);
        }
        #[cfg(windows)]
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(profile);
        }
    }
    dirs::home_dir().unwrap_or_default()
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

    /// Kill one entry (pid-reuse guarded). Returns false when the child was
    /// already reaped — nothing left to signal.
    fn kill_entry(pid: u32, child: &Arc<TokioMutex<tokio::process::Child>>, killed: &Arc<std::sync::atomic::AtomicBool>) -> bool {
        killed.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = child.try_lock() {
            // Pid-reuse guard: a reaped child's pid may already belong to
            // someone else — never signal a group we no longer own.
            match guard.try_wait() {
                Ok(Some(_)) => false,
                _ => {
                    kill_process_group(pid);
                    let _ = guard.start_kill();
                    true
                }
            }
        } else {
            // The runner holds the lock only while reaping post-EOF; that
            // window is tiny and the kill flag already settles the turn.
            kill_process_group(pid);
            true
        }
    }

    /// Kill **every** entry matching `key`: the map key (native session id or
    /// run id) and the recorded run id both match. One session resumed into
    /// several parallel runs must all die on a single stop, or the survivors
    /// keep streaming and fight the next run over the session file.
    pub fn kill(&self, key: &str) -> bool {
        let entries: Vec<(u32, Arc<TokioMutex<tokio::process::Child>>, Arc<std::sync::atomic::AtomicBool>)> =
            match self.0.lock() {
                Ok(map) => map
                    .iter()
                    .filter(|(k, e)| *k == key || e.run_id == key)
                    .map(|(_, e)| (e.pid, Arc::clone(&e.child), Arc::clone(&e.killed)))
                    .collect(),
                Err(_) => Vec::new(),
            };
        // No Iterator::any here: it short-circuits on the first true, which
        // would leave every later parallel run alive — the exact bug this
        // aggregate kill exists to fix.
        let mut killed_any = false;
        for (pid, child, killed) in &entries {
            killed_any |= Self::kill_entry(*pid, child, killed);
        }
        killed_any
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
    additional_dirs: Vec<String>,
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
        // Cap defensively: the list lands on a command line, and a
        // hand-edited db should not produce an argv bomb.
        additional_dirs: additional_dirs
            .into_iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty() && Path::new(d).is_absolute())
            .take(32)
            .collect(),
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
                args,
                result,
                patch,
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
                if let Some(args) = args {
                    payload["args"] = args;
                }
                if let Some(result) = result {
                    payload["result"] = result;
                }
                if patch {
                    payload["patch"] = Value::Bool(true);
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
                // The turn failed terminally: the frontend settles (send
                // button back to idle) on this event, so a still-running
                // CLI process would keep burning tokens invisibly while the
                // UI claims the session ended. Kill the process tree now —
                // the killed flag makes the runner's EOF path a no-op
                // (saw_error already settled the turn) and the registry
                // entry drains as usual.
                self.registry.kill(&self.run_id);
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
            EngineEvent::PermissionDenied {
                tool,
                path,
                message,
            } => {
                // Not terminal either: the CLI works around the denial and
                // the turn continues — the UI offers the grant alongside.
                state.push(
                    &self.sink,
                    &self.run_id,
                    &self.engine_id,
                    "permission_denied",
                    serde_json::json!({ "tool": tool, "path": path, "message": message }),
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
///
/// Settling must never depend on stdout EOF alone: on Windows any process in
/// the spawn chain (cmd shim → node launcher → CLI) or a helper the CLI
/// spawned can inherit the pipe's write handle and outlive the turn, so EOF
/// never arrives and the UI would stay "running" forever. `exit_watchdog`
/// therefore force-settles once the direct child is gone; whichever of the
/// two observes the settled flag first wins, the other becomes a no-op.
async fn run_reader(stdout: ChildStdout, ctx: Arc<RunContext>) {
    let state: SharedTurnState = Arc::new(Mutex::new(TurnState::new(
        ctx.preassigned_session_id.clone(),
    )));
    let settled = Arc::new(AtomicBool::new(false));
    tokio::spawn(exit_watchdog(
        Arc::clone(&ctx),
        Arc::clone(&state),
        Arc::clone(&settled),
    ));
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut state = state.lock();
        state.saw_any_output = true;
        let mut events = Vec::new();
        ctx.engine_impl.parse_line(trimmed, &mut events);
        for event in events {
            ctx.dispatch_event(&mut state, event);
        }
    }

    // Wait for exit
    let status = {
        let mut guard = ctx.child.lock().await;
        guard.wait().await.ok()
    };
    if !settled.swap(true, Ordering::SeqCst) {
        settle_exit(&ctx, &state, status);
    }
}

/// Per-run streaming state shared between [`run_reader`] and
/// [`exit_watchdog`]. parking_lot: a reader panic mid-line must not poison
/// the lock — the watchdog still has to settle the turn afterwards. Only
/// brief uncontended locks: the reader holds it per line, the watchdog only
/// inside `settle_exit`.
type SharedTurnState = Arc<parking_lot::Mutex<TurnState>>;

/// How long the exit watchdog waits after the child's exit before
/// force-settling. The normal reader path observes stdout EOF within
/// milliseconds of the child dying — the grace only has to cover draining
/// whatever the CLI already wrote into the pipe, so a few seconds is
/// generous even on slow machines.
const EXIT_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Settle the turn after the direct child exited: temp-file and registry
/// cleanup, stderr surfacing, and exactly one terminal done/error event.
/// Called by whichever of the reader and the exit watchdog wins the
/// `settled` flag; never runs twice.
fn settle_exit(ctx: &RunContext, state: &SharedTurnState, status: Option<ExitStatus>) {
    let mut state = state.lock();
    for path in &ctx.cleanup_files {
        let _ = std::fs::remove_file(path);
    }
    // Drain this run's registry entries: under the native session id after
    // rekey, and under the run id when the session id never arrived.
    if let Some(key) = state.native_session_id.clone() {
        ctx.registry.remove_if_pid(&key, ctx.pid);
    }
    ctx.registry.remove_if_pid(&ctx.run_id, ctx.pid);
    // omp writes some failures (upstream 403/5xx, quota exhaustion) to
    // stderr and then exits — sometimes cleanly, after a normal turn_end.
    // A non-empty stderr on a failed exit must reach the user even when a
    // done/error event already settled the turn; dropping it hides exactly
    // the errors the user cannot otherwise see.
    let stderr_tail = ctx
        .stderr_buf
        .lock()
        .map(|g| redact_secrets(g.trim()))
        .unwrap_or_default();
    let failed = status.map(|s| !s.success()).unwrap_or(true);
    if failed && !state.saw_error && !stderr_tail.is_empty() {
        state.push(
            &ctx.sink,
            &ctx.run_id,
            &ctx.engine_id,
            "warn",
            Value::String(format!("engine stderr: {stderr_tail}")),
        );
    }

    if !state.saw_done && !state.saw_error {
        let killed = ctx.killed.load(Ordering::SeqCst);
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
                message.push_str(&format!(": {stderr_tail}"));
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

/// Force-settle a run whose stdout never reached EOF. The direct child
/// exiting IS the turn's end — a CLI that finished its work must never leave
/// the session wedged in "running" (nor its registry entry pin a concurrency
/// slot) because a surviving pipe holder blocks EOF. Wait one grace period
/// so the normal reader path wins the race when EOF does arrive, then
/// settle; the losing reader's eventual EOF becomes a no-op.
async fn exit_watchdog(ctx: Arc<RunContext>, state: SharedTurnState, settled: Arc<AtomicBool>) {
    let status = {
        let mut guard = ctx.child.lock().await;
        guard.wait().await.ok()
    };
    tokio::time::sleep(EXIT_SETTLE_GRACE).await;
    if !settled.swap(true, Ordering::SeqCst) {
        settle_exit(&ctx, &state, status);
    }
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
        // Every user-granted directory rides along as a launch argument, so
        // a grant approved mid-conversation takes effect on the next send
        // (each send is a fresh process).
        state.db.granted_roots().unwrap_or_default(),
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
    let ctx = Arc::new(RunContext {
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
    });
    tokio::spawn(run_reader(stdout, Arc::clone(&ctx)));

    Ok(SendResult {
        run_id,
        session_id: launch.built.preassigned_session_id,
    })
}

#[tauri::command]
pub fn interrupt_session(state: tauri::State<'_, crate::AppState>, session_id: String) -> bool {
    state.processes.kill(&session_id)
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
            additional_dirs: Vec::new(),
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
        let e = claude::ClaudeEngine::new();
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
    fn codex_prompt_goes_through_stdin_not_argv() {
        // Windows resolves npm codex to a `.cmd` shim spawned via `cmd /c`;
        // cmd.exe cuts a multiline argument at the first newline, so only line
        // 1 ever reached the model. The prompt must ride stdin (`-`) verbatim.
        let e = codex::CodexEngine;
        let mut request = req(Some("auto"));
        request.prompt = "first line\nmodel = \"gpt-5\"\n%PATH%".to_string();
        let built = e.build_command(&request, "fake-bin").unwrap();
        let args: Vec<String> = built
            .command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"-".to_string()));
        assert!(!args.iter().any(|a| a.contains("first line")));
        assert_eq!(built.stdin_payload.as_deref(), Some(request.prompt.as_str()));

        let mut resume = req(Some("auto"));
        resume.session_id = Some("00000000-0000-0000-0000-000000000000".to_string());
        let built = e.build_command(&resume, "fake-bin").unwrap();
        let args: Vec<String> = built
            .command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"-".to_string()));
        assert_eq!(built.stdin_payload.as_deref(), Some("hi"));
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
    fn claude_passes_granted_dirs_as_add_dir() {
        let e = claude::ClaudeEngine::new();
        let mut r = req(Some("auto"));
        // Workspace itself, blanks and duplicates must not reach argv.
        r.additional_dirs = vec![
            "/data/shared".to_string(),
            "/tmp".to_string(),
            "   ".to_string(),
            "/data/shared".to_string(),
        ];
        let args = argv(&e, &r);
        let pairs: Vec<&[String]> = args
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .collect();
        assert_eq!(pairs.len(), 1, "{args:?}");
        assert_eq!(pairs[0][1], "/data/shared");

        // Other engines have no equivalent flag: the field stays inert.
        let codex_args = argv(&codex::CodexEngine, &r);
        assert!(!codex_args.iter().any(|a| a == "--add-dir"));
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

#[cfg(test)]
mod tool_args_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tool_args_value_drops_empty_and_parses_strings() {
        assert_eq!(parse_tool_args_value(&Value::Null), None);
        assert_eq!(parse_tool_args_value(&json!({})), None);
        assert_eq!(parse_tool_args_value(&json!([])), None);
        assert_eq!(
            parse_tool_args_value(&json!("{\"path\":\"a.ts\"}")),
            Some(json!({"path": "a.ts"}))
        );
        assert_eq!(
            parse_tool_args_value(&json!({"file_path": "a.ts"})),
            Some(json!({"file_path": "a.ts"}))
        );
        assert_eq!(
            parse_tool_args_value(&json!("plain command")),
            Some(json!("plain command"))
        );
    }

    #[test]
    fn tool_call_message_extracts_path_and_marks_patch() {
        match tool_call_message("Read", Some(&json!({"file_path": "src/a.ts"}))) {
            EngineEvent::Message {
                path, args, patch, ..
            } => {
                assert_eq!(path.as_deref(), Some("src/a.ts"));
                assert_eq!(args, Some(json!({"file_path": "src/a.ts"})));
                assert!(!patch);
            }
            _ => panic!("expected tool message"),
        }
        match tool_call_patch("Read", Some(&json!({"file_path": "src/a.ts"}))) {
            EngineEvent::Message { patch, .. } => assert!(patch),
            _ => panic!("expected patch"),
        }
    }
}

/// Decision table of `settle_exit` plus the reader's end-to-end wiring.
#[cfg(test)]
mod settle_exit_tests {
    use super::*;
    use crate::event_sink::Emit;

    /// Captures every flushed sink payload for assertions.
    #[derive(Default)]
    struct CapturingEmit(parking_lot::Mutex<Vec<String>>);

    impl Emit for CapturingEmit {
        fn emit_json(&self, _name: &str, raw_json: &str) {
            self.0.lock().push(raw_json.to_string());
        }
    }

    fn quick_child(exit_code: i32) -> Child {
        #[cfg(windows)]
        let mut command = {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/c").arg(format!("exit {exit_code}"));
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(format!("exit {exit_code}"));
            c
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.spawn().expect("spawn quick child")
    }

    /// A one-line stdout producer whose output is a codex turn.completed
    /// event: the reader path parses it, marks saw_done, and settles.
    fn echo_done_child() -> Child {
        #[cfg(windows)]
        let mut command = {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/c").arg("echo {\"type\":\"turn.completed\"}");
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg("echo '{\"type\":\"turn.completed\"}'");
            c
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        command.spawn().expect("spawn echo child")
    }

    fn test_ctx(captures: Arc<CapturingEmit>, child: Child) -> Arc<RunContext> {
        Arc::new(RunContext {
            sink: event_sink::EventSink::new(captures),
            registry: Arc::new(ProcessRegistry::default()),
            engine_impl: Box::new(codex::CodexEngine),
            engine_id: "codex".to_string(),
            run_id: "run-1".to_string(),
            pid: child.id().unwrap_or(0),
            preassigned_session_id: None,
            child: Arc::new(TokioMutex::new(child)),
            killed: Arc::new(AtomicBool::new(false)),
            cleanup_files: Vec::new(),
            stderr_buf: Arc::new(Mutex::new(String::new())),
        })
    }

    fn kinds(captures: &CapturingEmit) -> Vec<String> {
        captures
            .0
            .lock()
            .iter()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .flat_map(|value| -> Vec<String> {
                let Some(events) = value.as_array() else {
                    return Vec::new();
                };
                events
                    .iter()
                    .filter_map(|event| {
                        event
                            .get("kind")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .collect()
    }

    #[tokio::test]
    async fn clean_exit_without_done_pushes_done() {
        let captures = Arc::new(CapturingEmit::default());
        let ctx = test_ctx(Arc::clone(&captures), quick_child(0));
        let status = {
            let mut guard = ctx.child.lock().await;
            guard.wait().await.ok()
        };
        let state: SharedTurnState = Arc::new(parking_lot::Mutex::new(TurnState::new(None)));
        settle_exit(&ctx, &state, status);
        assert_eq!(kinds(&captures), vec!["done".to_string()]);
    }

    #[tokio::test]
    async fn failed_exit_pushes_error() {
        let captures = Arc::new(CapturingEmit::default());
        let ctx = test_ctx(Arc::clone(&captures), quick_child(3));
        let status = {
            let mut guard = ctx.child.lock().await;
            guard.wait().await.ok()
        };
        let state: SharedTurnState = Arc::new(parking_lot::Mutex::new(TurnState::new(None)));
        settle_exit(&ctx, &state, status);
        assert_eq!(kinds(&captures), vec!["error".to_string()]);
    }

    #[tokio::test]
    async fn already_done_state_pushes_no_terminal_event() {
        let captures = Arc::new(CapturingEmit::default());
        let ctx = test_ctx(Arc::clone(&captures), quick_child(0));
        let status = {
            let mut guard = ctx.child.lock().await;
            guard.wait().await.ok()
        };
        let state: SharedTurnState = Arc::new(parking_lot::Mutex::new(TurnState::new(None)));
        state.lock().saw_done = true;
        settle_exit(&ctx, &state, status);
        assert!(kinds(&captures).is_empty());
    }

    #[tokio::test]
    async fn killed_run_commits_as_done() {
        let captures = Arc::new(CapturingEmit::default());
        let ctx = test_ctx(Arc::clone(&captures), quick_child(0));
        ctx.killed.store(true, Ordering::SeqCst);
        let status = {
            let mut guard = ctx.child.lock().await;
            guard.wait().await.ok()
        };
        let state: SharedTurnState = Arc::new(parking_lot::Mutex::new(TurnState::new(None)));
        settle_exit(&ctx, &state, status);
        assert_eq!(kinds(&captures), vec!["done".to_string()]);
    }

    /// End-to-end: the reader parses the child's terminal line (done pushed
    /// from `turn.completed`), settles at EOF without duplicating the
    /// terminal event, and drains the registry entry.
    #[tokio::test]
    async fn run_reader_settles_once_and_drains_registry() {
        let captures = Arc::new(CapturingEmit::default());
        let ctx = test_ctx(Arc::clone(&captures), echo_done_child());
        ctx.registry.insert(
            "run-1".to_string(),
            ChildEntry {
                child: Arc::clone(&ctx.child),
                pid: ctx.pid,
                run_id: "run-1".to_string(),
                killed: Arc::clone(&ctx.killed),
            },
        );
        let stdout = {
            let mut guard = ctx.child.lock().await;
            guard.stdout.take().expect("stdout piped")
        };
        run_reader(stdout, Arc::clone(&ctx)).await;
        assert_eq!(kinds(&captures), vec!["done".to_string()]);
        assert_eq!(ctx.registry.len(), 0);
    }
}
