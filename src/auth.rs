use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTH_URL: &str = "https://login.xero.com/identity/connect/authorize";
const TOKEN_URL: &str = "https://identity.xero.com/connect/token";
const CONNECTIONS_URL: &str = "https://api.xero.com/connections";

const DEFAULT_SCOPES: &str = "openid profile email offline_access \
    accounting.invoices accounting.payments accounting.banktransactions accounting.manualjournals \
    accounting.contacts accounting.settings \
    accounting.reports.aged.read accounting.reports.balancesheet.read \
    accounting.reports.profitandloss.read accounting.reports.trialbalance.read";

const REDIRECT_URI: &str = "http://localhost:8765/callback";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub tenant_id: String,
    pub scopes: String,
}

pub fn tokens_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home)
        .join(".config")
        .join("mcp-server-xero")
        .join("tokens.json")
}

pub fn load_tokens() -> Option<StoredTokens> {
    let path = tokens_path();
    let mut file = std::fs::File::open(&path).ok()?;
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn save_tokens(tokens: &StoredTokens) -> Result<()> {
    let path = tokens_path();
    let json = serde_json::to_string_pretty(tokens)?;
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.write_all(json.as_bytes())?;
    // Restrict token file to owner-only access.
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Run the one-time browser authorization flow.
pub async fn authorize(client_id: &str, client_secret: &str, scopes: Option<&str>) -> Result<()> {
    let scopes = scopes.unwrap_or(DEFAULT_SCOPES);
    let state = {
        let mut buf = [0u8; 16];
        getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("Failed to generate random state: {e}"))?;
        buf.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    let auth_url = format!(
        "{AUTH_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}",
        urlencoding::encode(client_id),
        urlencoding::encode(REDIRECT_URI),
        urlencoding::encode(scopes),
    );

    // Start local server before opening browser.
    let listener = TcpListener::bind("127.0.0.1:8765").await
        .context("Failed to bind to port 8765 — is another instance running?")?;

    println!("Opening browser for Xero authorization...\n");
    println!("If the browser doesn't open, visit this URL:\n{auth_url}\n");

    if let Err(e) = open::that(&auth_url) {
        eprintln!("Could not open browser: {e}");
    }

    println!("Waiting for callback on {REDIRECT_URI} ...");

    // Wait for the callback.
    let (mut stream, _) = listener.accept().await?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the code and state from: GET /callback?code=...&state=... HTTP/1.1
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");

    let code = extract_param(path, "code");
    let returned_state = extract_param(path, "state");

    // Send a response to the browser.
    let (status, body) = if code.is_some() {
        ("200 OK", "<html><body><h2>Authorization successful!</h2><p>You can close this tab.</p></body></html>")
    } else {
        ("400 Bad Request", "<html><body><h2>Authorization failed</h2><p>No code received.</p></body></html>")
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    let code = code.ok_or_else(|| anyhow::anyhow!("No authorization code received"))?;

    if let Some(ref rs) = returned_state {
        if *rs != state {
            bail!("State mismatch — possible CSRF attack");
        }
    }

    println!("Authorization code received. Exchanging for tokens...");

    // Exchange code for tokens.
    let http = Client::new();
    let credentials = BASE64.encode(format!("{client_id}:{client_secret}"));

    let resp = http
        .post(TOKEN_URL)
        .header("Authorization", format!("Basic {credentials}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}",
            urlencoding::encode(&code),
            urlencoding::encode(REDIRECT_URI),
        ))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Token exchange failed ({status}): {body}");
    }

    let token_resp: TokenResponse = resp.json().await?;

    // Discover tenant ID.
    println!("Discovering connected organisation...");

    let connections: Vec<ConnectionInfo> = http
        .get(CONNECTIONS_URL)
        .header("Authorization", format!("Bearer {}", token_resp.access_token))
        .send()
        .await?
        .json()
        .await?;

    let tenant = connections
        .first()
        .ok_or_else(|| anyhow::anyhow!("No connected Xero organisation found"))?;

    let tokens = StoredTokens {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token.unwrap_or_default(),
        expires_at: now_secs() + token_resp.expires_in,
        tenant_id: tenant.tenant_id.clone(),
        scopes: scopes.to_string(),
    };

    save_tokens(&tokens)?;

    println!("\nAuthorization complete!");
    println!("  Organisation: {}", tenant.tenant_name.as_deref().unwrap_or("unknown"));
    println!("  Tenant ID:    {}", tenant.tenant_id);
    println!("  Tokens saved to: {}", tokens_path().display());
    println!("\nYou can now run the MCP server normally.");

    Ok(())
}

/// Refresh an access token using the stored refresh token.
pub async fn refresh_tokens(
    http: &Client,
    client_id: &str,
    client_secret: &str,
    stored: &StoredTokens,
) -> Result<StoredTokens> {
    let credentials = BASE64.encode(format!("{client_id}:{client_secret}"));

    let resp = http
        .post(TOKEN_URL)
        .header("Authorization", format!("Basic {credentials}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&refresh_token={}",
            urlencoding::encode(&stored.refresh_token),
        ))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Token refresh failed ({status}): {body}. Run `mcp-server-xero --auth` to re-authorize.");
    }

    let token_resp: TokenResponse = resp.json().await?;

    let updated = StoredTokens {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token.unwrap_or_else(|| stored.refresh_token.clone()),
        expires_at: now_secs() + token_resp.expires_in,
        tenant_id: stored.tenant_id.clone(),
        scopes: stored.scopes.clone(),
    };

    save_tokens(&updated)?;
    Ok(updated)
}

fn extract_param(path: &str, key: &str) -> Option<String> {
    let query = path.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            if k == key {
                return Some(urlencoding::decode(v).unwrap_or_default().into_owned());
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct ConnectionInfo {
    #[serde(rename = "tenantId")]
    tenant_id: String,
    #[serde(rename = "tenantName")]
    tenant_name: Option<String>,
}
