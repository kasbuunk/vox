# vox

Voice frontend for Claude Code sessions. A small Rust MCP server that lets the
Claude app (voice mode, phone, AirPods, driving) direct Claude Code sessions
running on a remote VM.

It is a Rust rebuild of the idea behind
[herdr-voice-mcp](https://github.com/lorenzkromer/herdr-voice-mcp) (TypeScript,
~3k lines). We read it for tool shapes only; we build against Herdr's socket
contract, not their code.

## Architecture

```
You (voice) → Claude app → Claude (Anthropic cloud)
   → HTTPS + token → vox (on the VM)
   → Unix socket, JSON lines → Herdr → panes running `claude`
```

- **Herdr** (Rust, separate project, herdrdev/herdr) is the runtime that holds
  terminal panes and agent sessions open. vox does not replace it; it needs a
  running Herdr server on the same host.
- **vox** is the thin adapter: MCP over streamable HTTP on one side, Herdr's
  Unix socket on the other, auth in between. No Node anywhere.

## Constraints and decisions

- **Rust only.** No npm/TypeScript. One binary.
- **Fire-and-forget.** Voice tool calls block the conversation, so every tool
  returns fast (seconds). `send` delivers a prompt and returns; the user polls
  with `list_agents` / `read` later. Never wait for a task to finish inside a
  tool call.
- **Short, speakable output.** Results are read aloud while driving. Plain
  text, one line per item, no diffs, no tables, no markdown. Truncate reads.
- **No Remote Control.** Company policy forbids it on the laptop; vox runs on
  the VM inside our own perimeter.
- **Reachability: the caller is Anthropic's cloud, not the phone.** Claude app
  custom connectors are invoked from Anthropic's infrastructure, so a VPN on
  the phone does not make the VM reachable. vox needs an HTTPS endpoint
  reachable from the internet (reverse proxy or tunnel), ideally restricted to
  Anthropic's published egress IP ranges.
- **Auth: two separate gates.**
  1. Claude Code on the VM is logged in to Anthropic once (`claude` login).
     vox does nothing with that.
  2. Phone→vox: a long random bearer token. Accepted as
     `Authorization: Bearer <token>` or as a secret path `/mcp/<token>`,
     because the Claude app connector dialog has no header field. Compared in
     constant time. Never log it. OAuth is a possible later upgrade.
- **TLS.** vox can terminate TLS itself (`--tls-cert`/`--tls-key`, rustls) or
  sit behind a TLS proxy on localhost. Without TLS the token travels in the
  clear, so plain HTTP is only allowed on loopback.
- **Scoping.** `spawn` only accepts working directories under configured
  `--allow-root` paths. Unattended sessions typically run with
  `--dangerously-skip-permissions`; directory scoping is the guardrail.

## Tools (MCP)

Five verbs, nothing more unless a real need shows up:

| Tool          | Herdr method(s)                         |
|---------------|-----------------------------------------|
| `list_agents` | `agent.list`                            |
| `send`        | `agent.prompt` (no wait)                |
| `spawn`       | `tab.create` (cwd) → `agent.start`      |
| `read`        | `agent.read` (`recent_unwrapped`, text) |
| `keys`        | `agent.send_keys`                       |

`spawn` takes Claude flags (`args`), e.g. `--dangerously-skip-permissions`,
`--model`, so flags are chosen at spawn time. Server-side `--default-arg`s are
prepended.

## Herdr socket contract

Verified against herdr 0.9.3 source (`src/api/schema/*.rs`).

- Socket: `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`.
- One JSON request per line: `{"id","method","params"}`; one response line:
  `{"id","result":{"type":...}}` or `{"id","error":{"code","message"}}`.
  Server closes after one pair (except `events.subscribe`). One connection
  per call.
- Agent targets: pane id (`w1:p1`) or agent name.
- Agent names: `^[a-z][a-z0-9_-]{0,31}$`, unique.
- `agent.start` needs an existing plain shell pane; `kind` is e.g. `claude`;
  `args` are appended to the executable. `timeout_ms` in (3000, 300000].
- Statuses: `idle`, `working`, `blocked`, `done`, `unknown`.
- Keys: herdr key-combo strings (`enter`, `esc`, `y`, `ctrl+c`, `shift+tab`).
- Types are internal to the herdr binary (no published crate), so the subset we
  need is copied into `src/herdr.rs`. Ignore unknown fields.

## Layout

- `src/main.rs` – CLI, config, HTTP server, TLS, auth middleware
- `src/herdr.rs` – Herdr socket client and wire types
- `src/tools.rs` – MCP tool definitions and voice formatting

## Working on it

```
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Tests must not need a running Herdr: use a fake Unix-socket server in tests.

## Out of scope for now

- Push notifications when an agent finishes (herdr-voice-mcp has a separate
  notifier via ntfy/Pushover). Next candidate after the core works.
- Stand-up summaries, delivery dedup with `request_id`, multi-machine.
