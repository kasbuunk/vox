//! MCP tools. Every tool returns within seconds and speaks short plain text,
//! because results are read aloud in voice mode.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::herdr::{self, AgentInfo, AgentStatus, Client};

const DEFAULT_READ_LINES: u32 = 30;
const MAX_READ_LINES: u32 = 200;
const MAX_READ_CHARS: usize = 4000;

#[derive(Debug)]
pub struct Settings {
    pub socket: PathBuf,
    /// Canonicalized directories `spawn` may start sessions in.
    pub allow_roots: Vec<PathBuf>,
    /// Arguments prepended to every spawned `claude` command line.
    pub default_args: Vec<String>,
    pub agent_kind: String,
}

#[derive(Clone)]
pub struct Vox {
    settings: Arc<Settings>,
    herdr: Arc<Client>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Only agents that need attention (blocked or done).
    #[serde(default)]
    pub attention_only: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendArgs {
    /// Agent name or pane id, e.g. "api" or "w1:p1".
    pub target: String,
    /// The prompt to type into the session.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpawnArgs {
    /// Absolute working directory for the new session. Must be under an allowed root.
    pub cwd: String,
    /// Agent name: lowercase letters, digits, dash, underscore; starts with a letter.
    /// Defaults to the directory name.
    #[serde(default)]
    pub name: Option<String>,
    /// Extra claude flags, e.g. ["--dangerously-skip-permissions", "--model", "opus"].
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// Agent name or pane id.
    pub target: String,
    /// How many recent lines to read (default 30, max 200).
    #[serde(default)]
    pub lines: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeysArgs {
    /// Agent name or pane id.
    pub target: String,
    /// Keys in order, e.g. ["y"], ["enter"], ["2", "enter"], ["esc"], ["ctrl+c"], ["shift+tab"].
    pub keys: Vec<String>,
}

fn ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn fail(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

#[tool_router]
impl Vox {
    pub fn new(settings: Settings) -> Self {
        Self {
            herdr: Arc::new(Client::new(&settings.socket)),
            settings: Arc::new(settings),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List Claude Code sessions with their status: working, blocked (waiting for an answer), done (finished, unseen), idle. Call this for 'how are things' or 'is it done'."
    )]
    async fn list_agents(&self, Parameters(args): Parameters<ListArgs>) -> CallToolResult {
        match self.herdr.agent_list().await {
            Ok(agents) => ok(format_agents(&agents, args.attention_only)),
            Err(e) => fail(format!("Could not list agents. {e}")),
        }
    }

    #[tool(
        description = "Type a prompt into a session and return immediately. Does not wait for the work to finish; check later with list_agents or read. Refused while the agent is blocked; answer it with keys first."
    )]
    async fn send(&self, Parameters(args): Parameters<SendArgs>) -> CallToolResult {
        if args.text.trim().is_empty() {
            return fail("Nothing to send.");
        }
        match self.herdr.agent_prompt(&args.target, &args.text).await {
            Ok(agent) => ok(format!("Sent to {}.", label(&agent))),
            Err(e) if e.code == "agent_blocked" => fail(format!(
                "{} is blocked on a question. Read it, then answer with keys.",
                args.target
            )),
            Err(e) => fail(format!("Not sent. {e}")),
        }
    }

    #[tool(
        description = "Start a new Claude Code session in a new tab, in the given directory. Pass claude flags in args, e.g. --dangerously-skip-permissions or --model. Then use send to give it work."
    )]
    async fn spawn(&self, Parameters(args): Parameters<SpawnArgs>) -> CallToolResult {
        let cwd = match self.check_cwd(&args.cwd) {
            Ok(p) => p,
            Err(msg) => return fail(msg),
        };
        let name = match args.name {
            Some(n) => n,
            None => name_from_dir(&cwd),
        };
        if !herdr::valid_agent_name(&name) {
            return fail(format!(
                "Bad name {name}. Use lowercase letters, digits and dashes, starting with a letter."
            ));
        }
        let cwd_str = cwd.to_string_lossy();
        let pane = match self.herdr.tab_create(&cwd_str, &name).await {
            Ok(p) => p,
            Err(e) => return fail(format!("Could not open a tab. {e}")),
        };
        let mut argv = self.settings.default_args.clone();
        argv.extend(args.args);
        match self
            .herdr
            .agent_start(&name, &self.settings.agent_kind, &pane, &argv)
            .await
        {
            Ok(agent) => ok(format!(
                "Started {} in {}. Ready for a prompt.",
                label(&agent),
                project(&cwd_str)
            )),
            Err(e) => fail(format!(
                "Tab {pane} opened but the agent did not start. {e}"
            )),
        }
    }

    #[tool(
        description = "Read the recent terminal output of a session, to hear what it did or what it is asking."
    )]
    async fn read(&self, Parameters(args): Parameters<ReadArgs>) -> CallToolResult {
        let lines = args
            .lines
            .unwrap_or(DEFAULT_READ_LINES)
            .clamp(1, MAX_READ_LINES);
        match self.herdr.agent_read(&args.target, lines).await {
            Ok(r) => ok(tidy_output(&r.text)),
            Err(e) => fail(format!("Could not read {}. {e}", args.target)),
        }
    }

    #[tool(
        description = "Send keystrokes to a session, e.g. to answer a yes/no or pick an option when it is blocked. Keys: printable characters, enter, esc, tab, up, down, ctrl+c, shift+tab."
    )]
    async fn keys(&self, Parameters(args): Parameters<KeysArgs>) -> CallToolResult {
        if args.keys.is_empty() {
            return fail("No keys given.");
        }
        match self.herdr.agent_send_keys(&args.target, &args.keys).await {
            Ok(()) => ok(format!(
                "Pressed {} on {}.",
                args.keys.join(" "),
                args.target
            )),
            Err(e) => fail(format!("Keys not sent. {e}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Vox {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("vox", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Drives Claude Code sessions on a remote machine for a user in voice mode, often driving. \
                 Keep replies to the user short. Never wait for long work: send, then check back with \
                 list_agents or read when asked.",
            )
    }
}

impl Vox {
    fn check_cwd(&self, cwd: &str) -> Result<PathBuf, String> {
        let path = Path::new(cwd);
        if !path.is_absolute() {
            return Err(format!("{cwd} is not an absolute path."));
        }
        let canon = path
            .canonicalize()
            .map_err(|_| format!("{cwd} does not exist."))?;
        if !canon.is_dir() {
            return Err(format!("{cwd} is not a directory."));
        }
        if self
            .settings
            .allow_roots
            .iter()
            .any(|r| canon.starts_with(r))
        {
            Ok(canon)
        } else {
            Err(format!("{cwd} is outside the allowed directories."))
        }
    }
}

fn label(a: &AgentInfo) -> String {
    a.name.clone().unwrap_or_else(|| a.pane_id.clone())
}

fn project(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

fn name_from_dir(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .skip_while(|c| !c.is_ascii_lowercase())
        .take(32)
        .collect();
    if name.is_empty() {
        name = "agent".into();
    }
    name
}

pub fn format_agents(agents: &[AgentInfo], attention_only: bool) -> String {
    let shown: Vec<&AgentInfo> = agents
        .iter()
        .filter(|a| a.agent.is_some() || a.name.is_some())
        .filter(|a| {
            !attention_only || matches!(a.agent_status, AgentStatus::Blocked | AgentStatus::Done)
        })
        .collect();
    if shown.is_empty() {
        return if attention_only {
            "Nothing needs attention.".into()
        } else {
            "No agents running.".into()
        };
    }
    shown
        .iter()
        .map(|a| {
            let mut line = format!("{}: {}", label(a), a.agent_status.as_str());
            if let Some(dir) = a.foreground_cwd.as_deref().or(a.cwd.as_deref()) {
                line.push_str(&format!(", in {}", project(dir)));
            }
            if let Some(t) = a.title.as_deref().or(a.terminal_title_stripped.as_deref())
                && !t.is_empty()
            {
                line.push_str(&format!(", {t}"));
            }
            line.push_str(&format!(" ({})", a.pane_id));
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drop blank lines and keep the tail within the speaking budget.
pub fn tidy_output(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return "The screen is empty.".into();
    }
    let joined = lines.join("\n");
    if joined.len() <= MAX_READ_CHARS {
        return joined;
    }
    let mut start = joined.len() - MAX_READ_CHARS;
    while !joined.is_char_boundary(start) {
        start += 1;
    }
    format!("(earlier output cut)\n{}", &joined[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake;
    use serde_json::{Value, json};

    fn agent(name: &str, status: &str) -> Value {
        json!({"terminal_id": "t", "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
               "focused": false, "revision": 1, "agent_status": status, "name": name,
               "agent": "claude", "cwd": "/home/me/src/vox"})
    }

    fn text(r: &CallToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            _ => panic!("not text"),
        }
    }

    fn vox(socket: &Path, roots: Vec<PathBuf>) -> Vox {
        Vox::new(Settings {
            socket: socket.to_path_buf(),
            allow_roots: roots,
            default_args: vec!["--dangerously-skip-permissions".into()],
            agent_kind: "claude".into(),
        })
    }

    #[test]
    fn formats_agents_for_speech() {
        let agents: Vec<AgentInfo> =
            serde_json::from_value(json!([agent("api", "working"), agent("web", "blocked"),]))
                .unwrap();
        assert_eq!(
            format_agents(&agents, false),
            "api: working, in vox (w1:p1)\nweb: blocked, in vox (w1:p1)"
        );
        assert_eq!(format_agents(&agents, true), "web: blocked, in vox (w1:p1)");
        assert_eq!(format_agents(&[], true), "Nothing needs attention.");
    }

    #[test]
    fn tidies_output() {
        assert_eq!(tidy_output("a  \n\n  \nb\n"), "a\nb");
        assert_eq!(tidy_output("\n\n"), "The screen is empty.");
        let long = "é".repeat(MAX_READ_CHARS);
        assert!(tidy_output(&long).starts_with("(earlier output cut)"));
    }

    #[test]
    fn derives_names_from_dirs() {
        assert_eq!(name_from_dir(Path::new("/x/My Repo")), "my-repo");
        assert_eq!(name_from_dir(Path::new("/x/2fast")), "fast");
        assert_eq!(name_from_dir(Path::new("/")), "agent");
    }

    #[tokio::test]
    async fn spawn_opens_tab_and_starts_claude_with_flags() {
        let fake = fake::spawn(Arc::new(|method, params| match method {
            "tab.create" => json!({"result": {"type": "tab_created",
                "tab": {}, "root_pane": {"pane_id": "w1:p7"}}}),
            "agent.start" => {
                let mut a = agent(params["name"].as_str().unwrap(), "idle");
                a["pane_id"] = json!("w1:p7");
                json!({"result": {"type": "agent_started", "agent": a, "argv": []}})
            }
            _ => panic!("unexpected {method}"),
        }));
        let root = std::env::temp_dir().canonicalize().unwrap();
        let work = root.join(format!("vox-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let v = vox(&fake.socket, vec![root]);
        let r = v
            .spawn(Parameters(SpawnArgs {
                cwd: work.to_string_lossy().into(),
                name: Some("api".into()),
                args: vec!["--model".into(), "opus".into()],
            }))
            .await;
        std::fs::remove_dir_all(&work).unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text(&r));
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].0, "tab.create");
        assert_eq!(calls[1].0, "agent.start");
        assert_eq!(calls[1].1["pane_id"], "w1:p7");
        assert_eq!(calls[1].1["kind"], "claude");
        assert_eq!(
            calls[1].1["args"],
            json!(["--dangerously-skip-permissions", "--model", "opus"])
        );
    }

    #[tokio::test]
    async fn spawn_refuses_dirs_outside_roots() {
        let fake = fake::spawn(Arc::new(|m, _| panic!("herdr must not be called: {m}")));
        let v = vox(&fake.socket, vec![PathBuf::from("/definitely/not/here")]);
        let r = v
            .spawn(Parameters(SpawnArgs {
                cwd: "/".into(),
                name: None,
                args: vec![],
            }))
            .await;
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("outside"));
    }

    #[tokio::test]
    async fn send_explains_blocked_agents() {
        let fake = fake::spawn(Arc::new(
            |_, _| json!({"error": {"code": "agent_blocked", "message": "blocked"}}),
        ));
        let v = vox(&fake.socket, vec![]);
        let r = v
            .send(Parameters(SendArgs {
                target: "api".into(),
                text: "run tests".into(),
            }))
            .await;
        assert_eq!(r.is_error, Some(true));
        assert!(text(&r).contains("answer with keys"));
    }
}
