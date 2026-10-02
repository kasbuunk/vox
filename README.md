# vox

Talk to your Claude Code sessions from the Claude app in voice mode.

vox is a small Rust MCP server. It runs on the machine where your Claude Code
sessions live (inside [Herdr](https://github.com/herdrdev/herdr)), and the
Claude app reaches it as a custom connector.

```
You (voice) → Claude app → Claude (Anthropic cloud)
   → HTTPS + token → vox → Herdr socket → claude panes
```

## Tools

| Tool          | Does                                                     |
|---------------|----------------------------------------------------------|
| `list_agents` | Sessions and status: working, blocked, done, idle        |
| `send`        | Type a prompt into a session, return immediately         |
| `spawn`       | New `claude` session in a directory, with optional flags |
| `read`        | Recent terminal output of a session                      |
| `keys`        | Keystrokes, e.g. `y`, `enter`, `esc`, `ctrl+c`           |

## Run

Prerequisites on the VM: Herdr running (`herdr status server`) and Claude
Code logged in.

```bash
cargo build --release
openssl rand -hex 32 > ~/.config/vox-token && chmod 600 ~/.config/vox-token

./target/release/vox \
  --token-file ~/.config/vox-token \
  --listen 0.0.0.0:8791 \
  --tls-cert cert.pem --tls-key key.pem \
  --allowed-host 203.0.113.10 \
  --allow-root ~/src \
  --default-arg=--dangerously-skip-permissions
```

- Plain HTTP is only allowed on loopback (for running behind a TLS proxy).
- `--allowed-host` is the host clients put in the URL (IP or hostname).
- `spawn` only accepts directories under `--allow-root`.
- If the cert is for an IP, the IP must be in the certificate's SAN.

## Connect the Claude app

Add a custom connector with URL `https://<host>:8791/mcp/<token>` and leave
the OAuth fields empty. (Clients that can send headers can use `/mcp` with
`Authorization: Bearer <token>` instead.) Treat the URL like a password.

**Reachability:** connector calls come from Anthropic's cloud, not from your
phone, so a VPN on the phone does not help. The endpoint must be reachable
from the internet, e.g. via a reverse proxy, ideally firewalled to
Anthropic's published egress IP ranges.

## Develop

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```
