use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_FILE_NAME: &str = "state.json";
const LOCK_FILE_NAME: &str = "state.lock";

pub fn run_from_env() -> i32 {
    let subcommand = match parse_subcommand(env::args().skip(1)) {
        Ok(subcommand) => subcommand,
        Err(ParseCommandError::Usage(message)) => {
            eprintln!("{message}");
            return 2;
        }
    };

    let state_dir = match env_path("HERDR_PLUGIN_STATE_DIR") {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let herdr_bin_path = match env_path("HERDR_BIN_PATH") {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };

    let runtime = RuntimeEnv {
        state_dir,
        event_json: env::var("HERDR_PLUGIN_EVENT_JSON").ok(),
        context_json: env::var("HERDR_PLUGIN_CONTEXT_JSON").ok(),
    };
    let herdr = CliHerdr {
        bin_path: herdr_bin_path,
    };

    match run(subcommand, &runtime, &herdr) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn run(subcommand: Subcommand, runtime: &RuntimeEnv, herdr: &dyn Herdr) -> Result<(), PluginError> {
    match subcommand {
        Subcommand::Toggle => toggle(runtime, herdr),
        Subcommand::Focused => focused(runtime, herdr),
        Subcommand::Closed => closed(runtime, herdr),
    }
}

fn toggle(runtime: &RuntimeEnv, herdr: &dyn Herdr) -> Result<(), PluginError> {
    let context_workspace_id = runtime
        .context_json
        .as_deref()
        .and_then(parse_context_workspace_id);

    let workspace_id = match context_workspace_id {
        Some(id) => id,
        None => {
            let ws = herdr.workspace_list()?;
            match ws.focused_workspace_id() {
                Some(id) => id.to_owned(),
                None => return Ok(()),
            }
        }
    };

    let snapshot = herdr.tab_list(&workspace_id)?;
    let current_tab_id = snapshot.focused_tab_id().map(str::to_owned);

    let Some(current_tab_id) = current_tab_id else {
        return Ok(());
    };

    let store = StateStore::new(&runtime.state_dir);
    let target = store.update(|mut state| {
        let memory = state.get_workspace(&workspace_id);
        let result = match memory {
            Memory::Empty => {
                state.set_workspace(
                    &workspace_id,
                    Memory::Current {
                        current: current_tab_id.clone(),
                        last: None,
                    },
                );
                None
            }
            Memory::Current { last: None, .. } => None,
            Memory::Current {
                current,
                last: Some(last),
            } => {
                if !snapshot.contains_tab(&last) || last == current_tab_id {
                    state.set_workspace(
                        &workspace_id,
                        Memory::Current {
                            current,
                            last: None,
                        },
                    );
                    None
                } else {
                    Some(last)
                }
            }
        };
        (state, result)
    })?;

    if let Some(target) = target {
        herdr.focus_tab(&target)?;
    }

    Ok(())
}

fn focused(runtime: &RuntimeEnv, herdr: &dyn Herdr) -> Result<(), PluginError> {
    let (event_tab_id, event_workspace_id) = match runtime
        .event_json
        .as_deref()
        .and_then(parse_event_tab_info)
    {
        Some(pair) => pair,
        None => return Ok(()),
    };

    let workspace_id = match event_workspace_id {
        Some(id) => id,
        None => {
            let ws = herdr.workspace_list()?;
            match ws.focused_workspace_id() {
                Some(id) => id.to_owned(),
                None => return Ok(()),
            }
        }
    };

    let snapshot = herdr.tab_list(&workspace_id)?;
    if snapshot.focused_tab_id() != Some(event_tab_id.as_str()) {
        return Ok(());
    }

    StateStore::new(&runtime.state_dir).update(|mut state| {
        let memory = state.get_workspace(&workspace_id);
        let next = match memory {
            Memory::Empty => Memory::Current {
                current: event_tab_id,
                last: None,
            },
            Memory::Current { current, last } if current == event_tab_id => {
                Memory::Current { current, last }
            }
            Memory::Current { current, .. } => Memory::Current {
                current: event_tab_id,
                last: Some(current),
            },
        };
        state.set_workspace(&workspace_id, next);
        (state, ())
    })?;

    Ok(())
}

fn closed(runtime: &RuntimeEnv, herdr: &dyn Herdr) -> Result<(), PluginError> {
    let (closed_tab_id, event_workspace_id) = match runtime
        .event_json
        .as_deref()
        .and_then(parse_event_tab_info)
    {
        Some(pair) => pair,
        None => return Ok(()),
    };

    let workspace_id = match event_workspace_id {
        Some(id) => id,
        None => {
            let ws = herdr.workspace_list()?;
            match ws.focused_workspace_id() {
                Some(id) => id.to_owned(),
                None => return Ok(()),
            }
        }
    };

    let snapshot = herdr.tab_list(&workspace_id)?;
    let focused_tab_id = snapshot.focused_tab_id().map(str::to_owned);

    StateStore::new(&runtime.state_dir).update(|mut state| {
        let memory = state.get_workspace(&workspace_id);
        let next = match memory {
            Memory::Empty => Memory::Empty,
            Memory::Current { current, last } => {
                let current = if current == closed_tab_id {
                    focused_tab_id.clone()
                } else {
                    Some(current)
                };

                match current {
                    Some(current) => {
                        let last =
                            last.filter(|last| last != &closed_tab_id && last != &current);
                        Memory::Current { current, last }
                    }
                    None => Memory::Empty,
                }
            }
        };
        state.set_workspace(&workspace_id, next);
        (state, ())
    })?;

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subcommand {
    Toggle,
    Focused,
    Closed,
}

enum ParseCommandError {
    Usage(String),
}

fn parse_subcommand<I>(args: I) -> Result<Subcommand, ParseCommandError>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let Some(raw_subcommand) = args.next() else {
        return Err(ParseCommandError::Usage(usage()));
    };
    if args.next().is_some() {
        return Err(ParseCommandError::Usage(usage()));
    }

    match raw_subcommand.as_str() {
        "toggle" => Ok(Subcommand::Toggle),
        "focused" => Ok(Subcommand::Focused),
        "closed" => Ok(Subcommand::Closed),
        "help" | "--help" | "-h" => Err(ParseCommandError::Usage(usage())),
        other => Err(ParseCommandError::Usage(format!(
            "unknown subcommand: {other}\n{}",
            usage()
        ))),
    }
}

fn usage() -> String {
    "usage: herdr-last-tab <toggle|focused|closed>".to_string()
}

fn env_path(name: &str) -> Result<PathBuf, PluginError> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            PluginError::new(format!(
                "{name} is not set; Herdr should provide this environment variable to plugin commands"
            ))
        })
}

struct RuntimeEnv {
    state_dir: PathBuf,
    event_json: Option<String>,
    context_json: Option<String>,
}

trait Herdr {
    fn workspace_list(&self) -> Result<WorkspaceSnapshot, PluginError>;
    fn tab_list(&self, workspace_id: &str) -> Result<TabSnapshot, PluginError>;
    fn focus_tab(&self, tab_id: &str) -> Result<(), PluginError>;
}

struct CliHerdr {
    bin_path: PathBuf,
}

impl Herdr for CliHerdr {
    fn workspace_list(&self) -> Result<WorkspaceSnapshot, PluginError> {
        let output = Command::new(&self.bin_path)
            .arg("workspace")
            .arg("list")
            .output()
            .map_err(|error| {
                PluginError::new(format!(
                    "failed to run workspace list ({}): {error}",
                    self.bin_path.display()
                ))
            })?;

        if !output.status.success() {
            return Err(command_failure("workspace list", &output));
        }

        parse_workspace_list_response(&output.stdout)
    }

    fn tab_list(&self, workspace_id: &str) -> Result<TabSnapshot, PluginError> {
        let output = Command::new(&self.bin_path)
            .arg("tab")
            .arg("list")
            .arg("--workspace")
            .arg(workspace_id)
            .output()
            .map_err(|error| {
                PluginError::new(format!(
                    "failed to run tab list ({}): {error}",
                    self.bin_path.display()
                ))
            })?;

        if !output.status.success() {
            return Err(command_failure("tab list", &output));
        }

        parse_tab_list_response(&output.stdout)
    }

    fn focus_tab(&self, tab_id: &str) -> Result<(), PluginError> {
        let output = Command::new(&self.bin_path)
            .arg("tab")
            .arg("focus")
            .arg(tab_id)
            .output()
            .map_err(|error| {
                PluginError::new(format!(
                    "failed to run tab focus {tab_id} ({}): {error}",
                    self.bin_path.display()
                ))
            })?;

        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure("tab focus", &output))
        }
    }
}

fn command_failure(command: &str, output: &Output) -> PluginError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        output.status.to_string()
    };
    PluginError::new(format!("{command} failed: {detail}"))
}

// --- Workspace types (for resolving current workspace) ---

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceSnapshot {
    workspaces: Vec<WorkspaceInfo>,
}

impl WorkspaceSnapshot {
    fn focused_workspace_id(&self) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|w| w.focused)
            .map(|w| w.workspace_id.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct WorkspaceInfo {
    workspace_id: String,
    focused: bool,
}

#[derive(Debug, Deserialize)]
struct WorkspaceListResponse {
    result: WorkspaceListResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkspaceListResult {
    WorkspaceList { workspaces: Vec<WorkspaceInfo> },
}

fn parse_workspace_list_response(stdout: &[u8]) -> Result<WorkspaceSnapshot, PluginError> {
    let value: Value = serde_json::from_slice(stdout).map_err(|error| {
        PluginError::new(format!("failed to parse workspace list JSON: {error}"))
    })?;

    if let Some(error) = herdr_error_message(&value) {
        return Err(PluginError::new(format!(
            "workspace list returned an error: {error}"
        )));
    }

    let response: WorkspaceListResponse = serde_json::from_value(value).map_err(|error| {
        PluginError::new(format!(
            "workspace list returned an unexpected response: {error}"
        ))
    })?;

    match response.result {
        WorkspaceListResult::WorkspaceList { workspaces } => {
            Ok(WorkspaceSnapshot { workspaces })
        }
    }
}

// --- Tab types ---

#[derive(Debug, Clone, PartialEq, Eq)]
struct TabSnapshot {
    tabs: Vec<TabInfo>,
}

impl TabSnapshot {
    fn focused_tab_id(&self) -> Option<&str> {
        self.tabs
            .iter()
            .find(|t| t.focused)
            .map(|t| t.tab_id.as_str())
    }

    fn contains_tab(&self, tab_id: &str) -> bool {
        self.tabs.iter().any(|t| t.tab_id == tab_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct TabInfo {
    tab_id: String,
    focused: bool,
}

#[derive(Debug, Deserialize)]
struct TabListResponse {
    result: TabListResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TabListResult {
    TabList { tabs: Vec<TabInfo> },
}

fn parse_tab_list_response(stdout: &[u8]) -> Result<TabSnapshot, PluginError> {
    let value: Value = serde_json::from_slice(stdout).map_err(|error| {
        PluginError::new(format!("failed to parse tab list JSON: {error}"))
    })?;

    if let Some(error) = herdr_error_message(&value) {
        return Err(PluginError::new(format!(
            "tab list returned an error: {error}"
        )));
    }

    let response: TabListResponse = serde_json::from_value(value).map_err(|error| {
        PluginError::new(format!(
            "tab list returned an unexpected response: {error}"
        ))
    })?;

    match response.result {
        TabListResult::TabList { tabs } => Ok(TabSnapshot { tabs }),
    }
}

// --- Shared helpers ---

fn herdr_error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    let code = error.get("code").and_then(Value::as_str);
    let message = error.get("message").and_then(Value::as_str);

    match (code, message) {
        (Some(code), Some(message)) => Some(format!("{code}: {message}")),
        (Some(code), None) => Some(code.to_string()),
        (None, Some(message)) => Some(message.to_string()),
        (None, None) => Some(error.to_string()),
    }
}

// --- Per-workspace state ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum Memory {
    Empty,
    Current {
        current: String,
        last: Option<String>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
struct WorkspaceTabState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_tab_id: Option<String>,
}

impl WorkspaceTabState {
    fn from_memory(memory: &Memory) -> Self {
        match memory {
            Memory::Empty => Self {
                current_tab_id: None,
                last_tab_id: None,
            },
            Memory::Current { current, last } => Self {
                current_tab_id: Some(current.clone()),
                last_tab_id: last.clone(),
            },
        }
    }

    fn into_memory(self) -> Memory {
        let current = self.current_tab_id.filter(|id| !id.is_empty());
        let last = self.last_tab_id.filter(|id| !id.is_empty());

        match current {
            Some(current) => {
                let last = last.filter(|l| l != &current);
                Memory::Current { current, last }
            }
            None => Memory::Empty,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PersistedState {
    #[serde(default)]
    workspaces: std::collections::HashMap<String, WorkspaceTabState>,
    #[serde(default)]
    updated_unix_ms: u64,
}

impl PersistedState {
    fn get_workspace(&self, workspace_id: &str) -> Memory {
        self.workspaces
            .get(workspace_id)
            .cloned()
            .unwrap_or_default()
            .into_memory()
    }

    fn set_workspace(&mut self, workspace_id: &str, memory: Memory) {
        self.workspaces
            .insert(workspace_id.to_owned(), WorkspaceTabState::from_memory(&memory));
        self.updated_unix_ms = current_unix_ms();
    }
}

struct StateStore {
    state_dir: PathBuf,
}

impl StateStore {
    fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
        }
    }

    fn update<T>(&self, change: impl FnOnce(PersistedState) -> (PersistedState, T)) -> Result<T, PluginError> {
        fs::create_dir_all(&self.state_dir).map_err(|error| {
            PluginError::new(format!(
                "failed to create plugin state directory {}: {error}",
                self.state_dir.display()
            ))
        })?;

        let _lock = StateLock::acquire(&self.state_dir.join(LOCK_FILE_NAME))?;
        let previous = read_state(&self.state_dir.join(STATE_FILE_NAME))?;
        let (next, result) = change(previous.clone());

        if next.workspaces != previous.workspaces {
            write_state(&self.state_dir.join(STATE_FILE_NAME), &next)?;
        }

        Ok(result)
    }
}

struct StateLock {
    file: File,
}

impl StateLock {
    fn acquire(path: &Path) -> Result<Self, PluginError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| {
                PluginError::new(format!(
                    "failed to open plugin state lock {}: {error}",
                    path.display()
                ))
            })?;

        file.lock_exclusive().map_err(|error| {
            PluginError::new(format!(
                "failed to lock plugin state {}: {error}",
                path.display()
            ))
        })?;

        Ok(Self { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn read_state(path: &Path) -> Result<PersistedState, PluginError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PersistedState::default());
        }
        Err(error) => {
            return Err(PluginError::new(format!(
                "failed to read plugin state {}: {error}",
                path.display()
            )));
        }
    };

    match serde_json::from_str::<PersistedState>(&contents) {
        Ok(state) => Ok(state),
        Err(_) => Ok(PersistedState::default()),
    }
}

fn write_state(path: &Path, state: &PersistedState) -> Result<(), PluginError> {
    let parent = path.parent().ok_or_else(|| {
        PluginError::new(format!(
            "plugin state path has no parent directory: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        PluginError::new(format!(
            "failed to create plugin state directory {}: {error}",
            parent.display()
        ))
    })?;

    let temp_path = parent.join(format!(
        ".{STATE_FILE_NAME}.tmp.{}.{}",
        std::process::id(),
        current_unix_ms()
    ));
    let mut temp_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(|error| {
            PluginError::new(format!(
                "failed to create temporary plugin state {}: {error}",
                temp_path.display()
            ))
        })?;
    serde_json::to_writer_pretty(&mut temp_file, state).map_err(|error| {
        PluginError::new(format!(
            "failed to serialize plugin state {}: {error}",
            temp_path.display()
        ))
    })?;
    temp_file.write_all(b"\n").map_err(|error| {
        PluginError::new(format!(
            "failed to write plugin state {}: {error}",
            temp_path.display()
        ))
    })?;
    temp_file.sync_all().map_err(|error| {
        PluginError::new(format!(
            "failed to sync plugin state {}: {error}",
            temp_path.display()
        ))
    })?;
    drop(temp_file);

    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        PluginError::new(format!(
            "failed to replace plugin state {}: {error}",
            path.display()
        ))
    })
}

fn parse_event_tab_info(raw: &str) -> Option<(String, Option<String>)> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let tab_id = string_at(&value, &["data", "tab_id"])
        .or_else(|| string_at(&value, &["tab_id"]))?;
    let workspace_id = string_at(&value, &["data", "workspace_id"])
        .or_else(|| string_at(&value, &["workspace_id"]));
    Some((tab_id, workspace_id))
}

fn parse_context_workspace_id(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    string_at(&value, &["workspace_id"])
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginError {
    message: String,
}

impl PluginError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PluginError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeHerdr {
        workspace_snapshot: WorkspaceSnapshot,
        tab_snapshot: TabSnapshot,
        focused_tabs: RefCell<Vec<String>>,
    }

    impl FakeHerdr {
        fn new(workspace_id: &str, tabs: Vec<(&str, bool)>) -> Self {
            Self {
                workspace_snapshot: WorkspaceSnapshot {
                    workspaces: vec![WorkspaceInfo {
                        workspace_id: workspace_id.to_string(),
                        focused: true,
                    }],
                },
                tab_snapshot: TabSnapshot {
                    tabs: tabs
                        .into_iter()
                        .map(|(tab_id, focused)| TabInfo {
                            tab_id: tab_id.to_string(),
                            focused,
                        })
                        .collect(),
                },
                focused_tabs: RefCell::new(Vec::new()),
            }
        }
    }

    impl Herdr for FakeHerdr {
        fn workspace_list(&self) -> Result<WorkspaceSnapshot, PluginError> {
            Ok(self.workspace_snapshot.clone())
        }

        fn tab_list(&self, _workspace_id: &str) -> Result<TabSnapshot, PluginError> {
            Ok(self.tab_snapshot.clone())
        }

        fn focus_tab(&self, tab_id: &str) -> Result<(), PluginError> {
            self.focused_tabs.borrow_mut().push(tab_id.to_string());
            Ok(())
        }
    }

    fn runtime(state_dir: PathBuf) -> RuntimeEnv {
        RuntimeEnv {
            state_dir,
            event_json: None,
            context_json: Some(r#"{"workspace_id":"w1"}"#.to_string()),
        }
    }

    fn temp_state_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "herdr-last-tab-{label}-{}-{}",
            std::process::id(),
            current_unix_ms()
        ));
        fs::create_dir_all(&path).expect("temp state directory should be created");
        path
    }

    fn seed_state(state_dir: &Path, workspace_id: &str, current: &str, last: Option<&str>) {
        let mut state = PersistedState::default();
        state.set_workspace(
            workspace_id,
            Memory::Current {
                current: current.to_string(),
                last: last.map(str::to_string),
            },
        );
        write_state(&state_dir.join(STATE_FILE_NAME), &state).expect("seed state");
    }

    #[test]
    fn focused_event_updates_current_and_last() {
        let state_dir = temp_state_dir("focused-transition");
        let mut rt = runtime(state_dir.clone());

        rt.event_json = Some(tab_event_json("w1:t1", "w1"));
        run(
            Subcommand::Focused,
            &rt,
            &FakeHerdr::new("w1", vec![("w1:t1", true), ("w1:t2", false)]),
        )
        .unwrap();

        rt.event_json = Some(tab_event_json("w1:t2", "w1"));
        run(
            Subcommand::Focused,
            &rt,
            &FakeHerdr::new("w1", vec![("w1:t1", false), ("w1:t2", true)]),
        )
        .unwrap();

        let state = read_state(&state_dir.join(STATE_FILE_NAME)).unwrap();
        let mem = state.get_workspace("w1");
        assert_eq!(
            mem,
            Memory::Current {
                current: "w1:t2".to_string(),
                last: Some("w1:t1".to_string()),
            }
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn toggle_focuses_last_tab() {
        let state_dir = temp_state_dir("toggle");
        seed_state(&state_dir, "w1", "w1:t2", Some("w1:t1"));
        let rt = runtime(state_dir.clone());
        let herdr = FakeHerdr::new("w1", vec![("w1:t2", true), ("w1:t1", false)]);

        run(Subcommand::Toggle, &rt, &herdr).unwrap();

        assert_eq!(herdr.focused_tabs.into_inner(), vec!["w1:t1".to_string()]);
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn toggle_seeds_empty_state_without_focusing() {
        let state_dir = temp_state_dir("toggle-seed");
        let rt = runtime(state_dir.clone());
        let herdr = FakeHerdr::new("w1", vec![("w1:t1", true)]);

        run(Subcommand::Toggle, &rt, &herdr).unwrap();

        assert!(herdr.focused_tabs.into_inner().is_empty());
        let state = read_state(&state_dir.join(STATE_FILE_NAME)).unwrap();
        assert_eq!(
            state.get_workspace("w1"),
            Memory::Current {
                current: "w1:t1".to_string(),
                last: None,
            }
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn toggle_clears_stale_last_tab() {
        let state_dir = temp_state_dir("toggle-stale");
        seed_state(&state_dir, "w1", "w1:t2", Some("w1:t1"));
        let rt = runtime(state_dir.clone());
        let herdr = FakeHerdr::new("w1", vec![("w1:t2", true)]);

        run(Subcommand::Toggle, &rt, &herdr).unwrap();

        assert!(herdr.focused_tabs.into_inner().is_empty());
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn closed_event_clears_last() {
        let state_dir = temp_state_dir("closed-last");
        seed_state(&state_dir, "w1", "w1:t2", Some("w1:t1"));
        let mut rt = runtime(state_dir.clone());
        rt.event_json = Some(tab_event_json("w1:t1", "w1"));

        run(
            Subcommand::Closed,
            &rt,
            &FakeHerdr::new("w1", vec![("w1:t2", true)]),
        )
        .unwrap();

        let state = read_state(&state_dir.join(STATE_FILE_NAME)).unwrap();
        assert_eq!(
            state.get_workspace("w1"),
            Memory::Current {
                current: "w1:t2".to_string(),
                last: None,
            }
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn per_workspace_isolation() {
        let state_dir = temp_state_dir("isolation");
        seed_state(&state_dir, "w1", "w1:t2", Some("w1:t1"));

        let state = read_state(&state_dir.join(STATE_FILE_NAME)).unwrap();
        assert_eq!(state.get_workspace("w2"), Memory::Empty);
        assert_eq!(
            state.get_workspace("w1"),
            Memory::Current {
                current: "w1:t2".to_string(),
                last: Some("w1:t1".to_string()),
            }
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    fn tab_event_json(tab_id: &str, workspace_id: &str) -> String {
        format!(
            r#"{{"event":"tab_focused","data":{{"type":"tab_focused","tab_id":"{tab_id}","workspace_id":"{workspace_id}"}}}}"#
        )
    }
}
