use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/drive";

#[derive(Deserialize)]
struct CredentialsFile {
    installed: InstalledCreds,
}

#[derive(Deserialize)]
struct InstalledCreds {
    client_id: String,
    client_secret: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Token {
    pub access_token: String,
    refresh_token: String,
    expiry: DateTime<Utc>,
}

impl Token {
    fn is_expired(&self) -> bool {
        Utc::now() >= self.expiry - Duration::seconds(60)
    }
}

pub async fn load_or_authenticate(
    http: &Client,
    creds_path: &str,
    token_path: &str,
) -> Result<Token> {
    if Path::new(token_path).exists() {
        let data = tokio::fs::read_to_string(token_path).await?;
        if let Ok(token) = serde_json::from_str::<Token>(&data) {
            if !token.is_expired() {
                return Ok(token);
            }
            let creds = load_creds(creds_path).await?;
            match do_refresh(http, &creds, &token).await {
                Ok(refreshed) => {
                    save_token(token_path, &refreshed).await?;
                    return Ok(refreshed);
                }
                Err(e) => eprintln!("Token refresh failed ({e}), re-authenticating ..."),
            }
        }
    }

    let creds = load_creds(creds_path).await?;
    let token = browser_flow(http, &creds).await?;
    save_token(token_path, &token).await?;
    Ok(token)
}

async fn load_creds(path: &str) -> Result<InstalledCreds> {
    let data = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Cannot read credentials file: {path}"))?;
    let f: CredentialsFile = serde_json::from_str(&data)?;
    Ok(f.installed)
}

async fn browser_flow(http: &Client, creds: &InstalledCreds) -> Result<Token> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    // Generate a random state token to protect against CSRF.
    let state = Uuid::new_v4().to_string();

    let mut auth_url = url::Url::parse(AUTH_URL)?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &creds.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("state", &state);

    let auth_url_str = auth_url.to_string();
    println!("Opening browser for Google authentication ...");
    println!("If it doesn't open automatically, visit:\n  {auth_url_str}");
    let _ = open::that(&auth_url_str);

    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
              <html><body><h2>Authentication successful!</h2>\
              <p>You can close this tab.</p></body></html>",
        )
        .await?;
    stream.flush().await.ok();

    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("Malformed HTTP request from browser")?;

    let parsed_url = url::Url::parse(&format!("http://localhost{path}"))?;
    let params: std::collections::HashMap<_, _> = parsed_url.query_pairs().collect();

    // Validate the state parameter to guard against CSRF.
    let returned_state = params
        .get("state")
        .map(|s| s.as_ref())
        .unwrap_or_default();
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF attack, aborting.");
    }

    let code = params
        .get("code")
        .map(|s| s.to_string())
        .context("No auth code in redirect URL")?;

    exchange_code(http, creds, &code, &redirect_uri).await
}

async fn exchange_code(
    http: &Client,
    creds: &InstalledCreds,
    code: &str,
    redirect_uri: &str,
) -> Result<Token> {
    let resp: serde_json::Value = http
        .post(TOKEN_URL)
        .form(&[
            ("code", code),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?
        .json()
        .await?;

    parse_token_response(resp, None)
}

async fn do_refresh(http: &Client, creds: &InstalledCreds, token: &Token) -> Result<Token> {
    let resp: serde_json::Value = http
        .post(TOKEN_URL)
        .form(&[
            ("refresh_token", token.refresh_token.as_str()),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?
        .json()
        .await?;

    parse_token_response(resp, Some(&token.refresh_token))
}

fn parse_token_response(resp: serde_json::Value, existing_refresh: Option<&str>) -> Result<Token> {
    if let Some(err) = resp.get("error") {
        // Surface only the error code, not the full response, to avoid leaking credentials.
        anyhow::bail!("Token error: {}", err.as_str().unwrap_or("unknown"));
    }
    let access_token = resp["access_token"]
        .as_str()
        .context("Missing access_token")?
        .to_string();
    let refresh_token = resp["refresh_token"]
        .as_str()
        .map(String::from)
        .or_else(|| existing_refresh.map(String::from))
        .context("Missing refresh_token")?;
    let expires_in = resp["expires_in"].as_i64().unwrap_or(3600);
    Ok(Token {
        access_token,
        refresh_token,
        expiry: Utc::now() + Duration::seconds(expires_in),
    })
}

async fn save_token(path: &str, token: &Token) -> Result<()> {
    let json = serde_json::to_string_pretty(token)?;

    // Write to a temp file first, then atomically rename to avoid corruption
    // if the process is killed mid-write.
    let tmp_path = format!("{path}.tmp");
    tokio::fs::write(&tmp_path, &json).await?;

    // Restrict permissions to owner-only before renaming into place.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&tmp_path, perms).await?;
    }

    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}
