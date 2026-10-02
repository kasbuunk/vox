//! Minimal client for the Herdr socket API.
//!
//! Wire format (herdr 0.9.3, `src/api/schema/*.rs`): Unix domain socket,
//! newline-delimited JSON, one request/response pair per connection.
//! Types below are the subset of Herdr's internal schema that vox needs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneReadResult {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

impl HerdrError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct AgentList {
    agents: Vec<AgentInfo>,
}

#[derive(Deserialize)]
struct AgentEnvelope {
    agent: AgentInfo,
}

#[derive(Deserialize)]
struct ReadEnvelope {
    read: PaneReadResult,
}

#[derive(Deserialize)]
struct TabCreated {
    root_pane: PaneInfo,
}

/// Default socket path: `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config/herdr/herdr.sock")
}

#[derive(Debug)]
pub struct Client {
    socket: PathBuf,
    next_id: AtomicU64,
}

const CALL_TIMEOUT: Duration = Duration::from_secs(15);
pub const START_TIMEOUT_MS: u64 = 30_000;

impl Client {
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
            next_id: AtomicU64::new(1),
        }
    }

    /// One request on a fresh connection; returns `result`.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T, HerdrError> {
        let id = format!("vox_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let fut = async {
            let mut stream = UnixStream::connect(&self.socket).await.map_err(|e| {
                HerdrError::new("socket_error", format!("herdr not reachable: {e}"))
            })?;
            let mut line =
                serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))
                    .map_err(|e| HerdrError::new("encode", e.to_string()))?;
            line.push(b'\n');
            stream
                .write_all(&line)
                .await
                .map_err(|e| HerdrError::new("socket_error", e.to_string()))?;
            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            let n = reader
                .read_line(&mut buf)
                .await
                .map_err(|e| HerdrError::new("socket_error", e.to_string()))?;
            if n == 0 {
                return Err(HerdrError::new(
                    "connection_closed",
                    "herdr closed without a response",
                ));
            }
            let resp: Response = serde_json::from_str(&buf)
                .map_err(|e| HerdrError::new("bad_response", e.to_string()))?;
            if let Some(err) = resp.error {
                return Err(HerdrError::new(&err.code, err.message));
            }
            let result = resp.result.unwrap_or(Value::Null);
            serde_json::from_value(result)
                .map_err(|e| HerdrError::new("bad_response", e.to_string()))
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| HerdrError::new("timeout", format!("{method}: no response")))?
    }

    pub async fn agent_list(&self) -> Result<Vec<AgentInfo>, HerdrError> {
        let r: AgentList = self.call("agent.list", json!({}), CALL_TIMEOUT).await?;
        Ok(r.agents)
    }

    /// Submit a prompt without waiting for the agent to finish.
    pub async fn agent_prompt(&self, target: &str, text: &str) -> Result<AgentInfo, HerdrError> {
        let r: AgentEnvelope = self
            .call(
                "agent.prompt",
                json!({"target": target, "text": text}),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(r.agent)
    }

    pub async fn agent_read(&self, target: &str, lines: u32) -> Result<PaneReadResult, HerdrError> {
        let params = json!({
            "target": target,
            "source": "recent_unwrapped",
            "lines": lines,
            "format": "text",
            "strip_ansi": true,
        });
        let r: ReadEnvelope = self.call("agent.read", params, CALL_TIMEOUT).await?;
        Ok(r.read)
    }

    pub async fn agent_send_keys(&self, target: &str, keys: &[String]) -> Result<(), HerdrError> {
        let _: Value = self
            .call(
                "agent.send_keys",
                json!({"target": target, "keys": keys}),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    /// Open a new tab in `cwd` and return its root (shell) pane id.
    pub async fn tab_create(&self, cwd: &str, label: &str) -> Result<String, HerdrError> {
        let r: TabCreated = self
            .call(
                "tab.create",
                json!({"cwd": cwd, "label": label, "focus": false}),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(r.root_pane.pane_id)
    }

    pub async fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> Result<AgentInfo, HerdrError> {
        let params = json!({
            "name": name,
            "kind": kind,
            "pane_id": pane_id,
            "args": args,
            "timeout_ms": START_TIMEOUT_MS,
        });
        let r: AgentEnvelope = self
            .call(
                "agent.start",
                params,
                Duration::from_millis(START_TIMEOUT_MS) + CALL_TIMEOUT,
            )
            .await?;
        Ok(r.agent)
    }
}

/// Herdr agent names: `^[a-z][a-z0-9_-]{0,31}$`.
pub fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

#[cfg(test)]
pub mod fake {
    //! A fake Herdr server for tests: answers each request via a closure.

    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;

    pub type Handler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

    pub struct Fake {
        pub dir: PathBuf,
        pub socket: PathBuf,
        pub calls: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `handler` returns either `{"result": ...}` or `{"error": ...}`.
    pub fn spawn(handler: Handler) -> Fake {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vox-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap().to_string();
                    let params = req["params"].clone();
                    recorded
                        .lock()
                        .unwrap()
                        .push((method.clone(), params.clone()));
                    let mut resp = handler(&method, &params);
                    resp["id"] = req["id"].clone();
                    let mut out = serde_json::to_vec(&resp).unwrap();
                    out.push(b'\n');
                    reader.get_mut().write_all(&out).await.unwrap();
                });
            }
        });
        Fake { dir, socket, calls }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn agent_names() {
        assert!(valid_agent_name("api"));
        assert!(valid_agent_name("a1-b_c"));
        assert!(!valid_agent_name("1abc"));
        assert!(!valid_agent_name("Api"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name(&"a".repeat(33)));
    }

    #[tokio::test]
    async fn parses_agent_list_and_ignores_unknown_fields() {
        let fake = fake::spawn(Arc::new(|method, _| {
            assert_eq!(method, "agent.list");
            json!({"result": {"type": "agent_list", "agents": [{
                "terminal_id": "t1", "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
                "focused": false, "revision": 3, "agent_status": "working",
                "name": "api", "agent": "claude", "brand_new_field": 42
            }]}})
        }));
        let agents = Client::new(&fake.socket).agent_list().await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_status, AgentStatus::Working);
        assert_eq!(agents[0].name.as_deref(), Some("api"));
    }

    #[tokio::test]
    async fn surfaces_herdr_errors() {
        let fake = fake::spawn(Arc::new(
            |_, _| json!({"error": {"code": "agent_blocked", "message": "agent is blocked"}}),
        ));
        let err = Client::new(&fake.socket)
            .agent_prompt("api", "hi")
            .await
            .unwrap_err();
        assert_eq!(err.code, "agent_blocked");
    }

    #[tokio::test]
    async fn missing_socket_is_a_clear_error() {
        let err = Client::new("/nonexistent/herdr.sock")
            .agent_list()
            .await
            .unwrap_err();
        assert_eq!(err.code, "socket_error");
    }
}
