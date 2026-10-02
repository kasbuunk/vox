mod herdr;
mod tools;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use clap::Parser;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use subtle::ConstantTimeEq;

const MIN_TOKEN_LEN: usize = 32;

/// Voice frontend for Claude Code: an MCP server that drives Herdr sessions.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Address to listen on.
    #[arg(long, env = "VOX_LISTEN", default_value = "127.0.0.1:8791")]
    listen: SocketAddr,

    /// File holding the bearer token (at least 32 characters). Alternatively set VOX_TOKEN.
    #[arg(long, env = "VOX_TOKEN_FILE")]
    token_file: Option<PathBuf>,

    /// PEM certificate chain. With --tls-key, vox terminates TLS itself.
    #[arg(long, env = "VOX_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// PEM private key.
    #[arg(long, env = "VOX_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Directory spawn may start sessions in. Repeatable.
    #[arg(long = "allow-root", env = "VOX_ALLOW_ROOTS", value_delimiter = ',')]
    allow_roots: Vec<PathBuf>,

    /// Argument prepended to every spawned claude command. Repeatable.
    #[arg(long = "default-arg", allow_hyphen_values = true)]
    default_args: Vec<String>,

    /// Extra accepted Host header value (IP or hostname clients use). Repeatable.
    #[arg(
        long = "allowed-host",
        env = "VOX_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    allowed_hosts: Vec<String>,

    /// Herdr socket path. Defaults to $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock.
    #[arg(long, env = "VOX_HERDR_SOCKET")]
    herdr_socket: Option<PathBuf>,

    /// Herdr agent kind to start.
    #[arg(long, default_value = "claude")]
    agent_kind: String,
}

fn load_token(cli: &Cli) -> Result<String> {
    let token = match (&cli.token_file, std::env::var("VOX_TOKEN")) {
        (Some(path), _) => std::fs::read_to_string(path)
            .with_context(|| format!("reading token file {}", path.display()))?,
        (None, Ok(t)) => t,
        (None, Err(_)) => bail!("no token: pass --token-file or set VOX_TOKEN"),
    };
    let token = token.trim().to_string();
    if token.len() < MIN_TOKEN_LEN {
        bail!("token must be at least {MIN_TOKEN_LEN} characters");
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c))
    {
        bail!("token must be URL-safe (letters, digits, - _ . ~)");
    }
    Ok(token)
}

/// Accept `Authorization: Bearer <token>` on `/mcp`, or the secret path `/mcp/<token>`
/// (the Claude app connector dialog has no header field).
fn authorized(token: &str, path: &str, auth_header: Option<&str>) -> bool {
    let eq = |given: &str| bool::from(given.as_bytes().ct_eq(token.as_bytes()));
    match path.strip_prefix("/mcp") {
        Some("") | Some("/") => auth_header
            .and_then(|h| h.strip_prefix("Bearer "))
            .is_some_and(eq),
        Some(rest) => rest
            .strip_prefix('/')
            .map(|s| s.trim_end_matches('/'))
            .is_some_and(eq),
        None => false,
    }
}

async fn auth(State(token): State<Arc<String>>, req: Request, next: Next) -> Response {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if authorized(&token, req.uri().path(), header) {
        next.run(req).await
    } else {
        tracing::warn!(
            path_len = req.uri().path().len(),
            "rejected unauthorized request"
        );
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let token = Arc::new(load_token(&cli)?);
    let tls = cli.tls_cert.is_some();
    if !tls && !cli.listen.ip().is_loopback() {
        bail!(
            "refusing to serve the token over plain HTTP on {}: use --tls-cert/--tls-key or a loopback address behind a TLS proxy",
            cli.listen
        );
    }

    let mut allow_roots = Vec::new();
    for root in &cli.allow_roots {
        allow_roots.push(
            root.canonicalize()
                .with_context(|| format!("allow-root {}", root.display()))?,
        );
    }
    if allow_roots.is_empty() {
        tracing::warn!("no --allow-root set: spawn will refuse every directory");
    }

    let settings = tools::Settings {
        socket: cli
            .herdr_socket
            .clone()
            .unwrap_or_else(herdr::default_socket_path),
        allow_roots,
        default_args: cli.default_args.clone(),
        agent_kind: cli.agent_kind.clone(),
    };
    tracing::info!(socket = %settings.socket.display(), roots = ?settings.allow_roots, "herdr");

    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if !cli.listen.ip().is_unspecified() {
        hosts.push(cli.listen.ip().to_string());
    }
    hosts.extend(cli.allowed_hosts.iter().cloned());
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(hosts);

    let vox = tools::Vox::new(settings);
    let service = StreamableHttpService::new(
        move || Ok(vox.clone()),
        LocalSessionManager::default().into(),
        config,
    );
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(token, auth));

    let scheme = if tls { "https" } else { "http" };
    tracing::info!("vox listening on {scheme}://{}/mcp", cli.listen);
    match (&cli.tls_cert, &cli.tls_key) {
        (Some(cert), Some(key)) => {
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .context("loading TLS certificate")?;
            axum_server::bind_rustls(cli.listen, rustls)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            let listener = tokio::net::TcpListener::bind(cli.listen).await?;
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::authorized;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn bearer_header_on_mcp() {
        assert!(authorized(T, "/mcp", Some(&format!("Bearer {T}"))));
        assert!(!authorized(T, "/mcp", Some("Bearer wrong")));
        assert!(!authorized(T, "/mcp", None));
    }

    #[test]
    fn secret_path() {
        assert!(authorized(T, &format!("/mcp/{T}"), None));
        assert!(authorized(T, &format!("/mcp/{T}/"), None));
        assert!(!authorized(T, "/mcp/nope", None));
        assert!(!authorized(T, &format!("/mcp/{T}/extra"), None));
        assert!(!authorized(T, &format!("/mcpx/{T}"), None));
        assert!(!authorized(T, &format!("/other/{T}"), None));
    }
}
