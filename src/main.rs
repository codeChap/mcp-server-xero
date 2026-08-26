mod api;
mod auth;
mod config;
mod params;
mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing::info;
use tracing_subscriber::EnvFilter;

use api::{AuthMode, XeroClient};
use server::XeroServer;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Handle --auth flag: run the one-time browser authorization flow.
    if args.iter().any(|a| a == "--auth") {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();

        let cfg = config::load()?;
        auth::authorize(&cfg.client_id, &cfg.client_secret, cfg.scopes.as_deref()).await?;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    info!("loading config");
    let cfg = config::load()?;

    // Determine auth mode: if tokens.json exists, use auth code flow.
    // Otherwise, fall back to client_credentials (Custom Connection).
    let mode = if let Some(tokens) = auth::load_tokens() {
        info!("using stored tokens (authorization code flow)");
        AuthMode::AuthorizationCode { tokens }
    } else if auth::tokens_path().exists() {
        // A token file is present but could not be parsed. Silently dropping to
        // client_credentials fails confusingly (Web App apps reject that grant),
        // so surface the real problem instead of masking it.
        anyhow::bail!(
            "Token file {} exists but could not be parsed. \
             Re-run `mcp-server-xero --auth` to re-authorize.",
            auth::tokens_path().display()
        );
    } else {
        info!("using client_credentials flow (Custom Connection)");
        AuthMode::ClientCredentials {
            scopes: cfg.scopes.clone(),
        }
    };

    info!("initializing Xero API client");
    let client = XeroClient::new(cfg.client_id, cfg.client_secret, mode);

    let server = XeroServer::new(client);

    info!("starting MCP server via stdio");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
