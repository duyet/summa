use serde::Deserialize;
use worker::{Env, Request, Response, Result};

use crate::types::{new_id, sha256_hex, timing_safe_eq, TOKEN_PREFIX};

#[derive(Clone)]
pub struct ApiKeyAuth {
    pub account_id: String,
    pub api_key_id: String,
}

#[derive(Clone)]
pub struct SessionAuth {
    pub account_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Unauthorized,
    Unavailable,
}

#[derive(Deserialize)]
struct AccountRow {
    id: String,
    #[allow(dead_code)]
    clerk_user_id: Option<String>,
    name: String,
    created_at: String,
}

#[derive(Deserialize)]
struct ApiKeyRow {
    id: String,
    account_id: String,
    name: String,
    token_prefix: String,
    created_at: String,
    revoked_at: Option<String>,
}

pub fn token_from_headers(x_summa: Option<&str>, authorization: Option<&str>) -> String {
    if let Some(t) = x_summa.map(str::trim).filter(|s| !s.is_empty()) {
        return t.to_string();
    }
    if let Some(h) = authorization.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(t) = h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
        {
            return t.trim().to_string();
        }
        return h.to_string();
    }
    String::new()
}

pub fn extract_bearer(req: &Request) -> String {
    let x_summa = req.headers().get("X-Summa-Token").ok().flatten();
    let authorization = req.headers().get("Authorization").ok().flatten();
    token_from_headers(x_summa.as_deref(), authorization.as_deref())
}

fn unauthorized() -> Result<Response> {
    Response::from_json(&serde_json::json!({"error": "unauthorized"})).map(|r| r.with_status(401))
}

pub async fn require_api_key(
    req: &Request,
    env: &Env,
) -> std::result::Result<ApiKeyAuth, AuthError> {
    let token = extract_bearer(req);
    if !token.starts_with(TOKEN_PREFIX) {
        return Err(AuthError::Unauthorized);
    }
    let hash = sha256_hex(&token);
    let db = env.d1("DB").map_err(|_| AuthError::Unavailable)?;
    let row = db
        .prepare(
            "SELECT id, account_id, name, token_prefix, created_at, revoked_at FROM api_keys WHERE token_hash = ? AND revoked_at IS NULL",
        )
        .bind(&[hash.into()])
        .map_err(|_| AuthError::Unavailable)?
        .first::<ApiKeyRow>(None)
        .await
        .map_err(|_| AuthError::Unavailable)?;
    let Some(row) = row else {
        return Err(AuthError::Unauthorized);
    };
    let _ = row.name;
    Ok(ApiKeyAuth {
        account_id: row.account_id,
        api_key_id: row.id,
    })
}

pub async fn require_session(
    req: &Request,
    env: &Env,
) -> std::result::Result<SessionAuth, AuthError> {
    let token = extract_bearer(req);
    if token.is_empty() {
        return Err(AuthError::Unauthorized);
    }
    let bootstrap = opt_secret(env, "BOOTSTRAP_TOKEN");
    if !bootstrap.is_empty() && timing_safe_eq(&token, &bootstrap) {
        let account = ensure_owner_account(env)
            .await
            .map_err(|_| AuthError::Unavailable)?;
        return Ok(SessionAuth {
            account_id: account.id,
        });
    }
    if let Some(clerk) = verify_clerk_me(&token, env).await {
        let account = ensure_clerk_account(env, &clerk.0, &clerk.1)
            .await
            .map_err(|_| AuthError::Unavailable)?;
        return Ok(SessionAuth {
            account_id: account.id,
        });
    }
    Err(AuthError::Unauthorized)
}

pub fn auth_error_status(err: AuthError) -> u16 {
    match err {
        AuthError::Unauthorized => 401,
        AuthError::Unavailable => 503,
    }
}

pub fn auth_error_response(err: AuthError) -> Result<Response> {
    match err {
        AuthError::Unauthorized => unauthorized(),
        AuthError::Unavailable => Response::from_json(&serde_json::json!({
            "error": "auth unavailable"
        }))
        .map(|r| r.with_status(auth_error_status(err))),
    }
}

pub fn opt_secret(env: &Env, name: &str) -> String {
    env.secret(name)
        .map(|s| s.to_string())
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn opt_var(env: &Env, name: &str) -> String {
    env.var(name)
        .map(|s| s.to_string())
        .unwrap_or_default()
        .trim()
        .to_string()
}

async fn ensure_owner_account(env: &Env) -> Result<AccountRow> {
    let db = env.d1("DB")?;
    if let Some(row) = db
        .prepare("SELECT id, clerk_user_id, name, created_at FROM accounts ORDER BY created_at ASC LIMIT 1")
        .first::<AccountRow>(None)
        .await?
    {
        return Ok(row);
    }
    let account = AccountRow {
        id: format!(
            "{}-{}-{}-{}-{}",
            &new_id()[..8],
            &new_id()[..4],
            &new_id()[..4],
            &new_id()[..4],
            &new_id()[..12]
        ),
        clerk_user_id: None,
        name: "owner".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    db.prepare("INSERT INTO accounts (id, clerk_user_id, name, created_at) VALUES (?, ?, ?, ?)")
        .bind(&[
            account.id.clone().into(),
            worker::wasm_bindgen::JsValue::NULL,
            account.name.clone().into(),
            account.created_at.clone().into(),
        ])?
        .run()
        .await?;
    Ok(account)
}

async fn ensure_clerk_account(env: &Env, clerk_user_id: &str, name: &str) -> Result<AccountRow> {
    let db = env.d1("DB")?;
    if let Some(row) = db
        .prepare("SELECT id, clerk_user_id, name, created_at FROM accounts WHERE clerk_user_id = ?")
        .bind(&[clerk_user_id.into()])?
        .first::<AccountRow>(None)
        .await?
    {
        return Ok(row);
    }
    let account = AccountRow {
        id: new_id(),
        clerk_user_id: Some(clerk_user_id.to_string()),
        name: name.chars().take(80).collect(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    db.prepare("INSERT INTO accounts (id, clerk_user_id, name, created_at) VALUES (?, ?, ?, ?)")
        .bind(&[
            account.id.clone().into(),
            clerk_user_id.into(),
            account.name.clone().into(),
            account.created_at.clone().into(),
        ])?
        .run()
        .await?;
    Ok(account)
}

pub async fn is_owner_account(env: &Env, account_id: &str) -> Result<bool> {
    let db = env.d1("DB")?;
    let row = db
        .prepare("SELECT id, clerk_user_id, name, created_at FROM accounts ORDER BY created_at ASC LIMIT 1")
        .first::<AccountRow>(None)
        .await?;
    Ok(row.map(|r| r.id == account_id).unwrap_or(false))
}

pub async fn mint_api_key(
    env: &Env,
    account_id: &str,
    name: &str,
) -> Result<(String, String, String)> {
    let id = new_id();
    let mut raw = [0u8; 32];
    let _ = getrandom::fill(&mut raw);
    let token = format!("{TOKEN_PREFIX}{}", hex::encode(raw));
    let prefix: String = token.chars().take(14).collect();
    let token_hash = sha256_hex(&token);
    let created = chrono::Utc::now().to_rfc3339();
    let key_name = if name.trim().is_empty() {
        "default".to_string()
    } else {
        name.trim().chars().take(80).collect()
    };
    env.d1("DB")?
        .prepare(
            "INSERT INTO api_keys (id, account_id, name, token_hash, token_prefix, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&[
            id.clone().into(),
            account_id.into(),
            key_name.into(),
            token_hash.into(),
            prefix.clone().into(),
            created.into(),
        ])?
        .run()
        .await?;
    Ok((id, token, prefix))
}

pub async fn list_api_keys(env: &Env, account_id: &str) -> Result<Vec<serde_json::Value>> {
    let rows = env
        .d1("DB")?
        .prepare(
            "SELECT id, account_id, name, token_prefix, created_at, revoked_at FROM api_keys WHERE account_id = ? ORDER BY created_at DESC",
        )
        .bind(&[account_id.into()])?
        .all()
        .await?
        .results::<ApiKeyRow>()?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "name": r.name,
                "prefix": r.token_prefix,
                "created_at": r.created_at,
                "revoked_at": r.revoked_at,
            })
        })
        .collect())
}

pub async fn revoke_api_key(env: &Env, account_id: &str, key_id: &str) -> Result<bool> {
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct KeyId {
        id: String,
    }
    let db = env.d1("DB")?;
    let existing = db
        .prepare("SELECT id FROM api_keys WHERE id = ? AND account_id = ? AND revoked_at IS NULL")
        .bind(&[key_id.into(), account_id.into()])?
        .first::<KeyId>(None)
        .await?;
    if existing.is_none() {
        return Ok(false);
    }
    let now = chrono::Utc::now().to_rfc3339();
    db.prepare(
        "UPDATE api_keys SET revoked_at = ? WHERE id = ? AND account_id = ? AND revoked_at IS NULL",
    )
    .bind(&[now.into(), key_id.into(), account_id.into()])?
    .run()
    .await?;
    Ok(true)
}

async fn verify_clerk_me(token: &str, env: &Env) -> Option<(String, String)> {
    let secret = opt_secret(env, "CLERK_SECRET_KEY");
    if secret.is_empty() {
        return None;
    }
    let headers = worker::Headers::new();
    let _ = headers.set("Authorization", &format!("Bearer {token}"));
    let _ = headers.set("Clerk-Secret-Key", &secret);
    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Get);
    init.with_headers(headers);
    for url in [
        "https://api.clerk.com/v1/me",
        "https://api.clerk.com/v1/users/me",
    ] {
        let req = worker::Request::new_with_init(url, &init).ok()?;
        let mut resp = worker::Fetch::Request(req).send().await.ok()?;
        if resp.status_code() != 200 {
            continue;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        let id = v.get("id").and_then(|x| x.as_str())?;
        let name = v
            .get("username")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("first_name").and_then(|x| x.as_str()))
            .unwrap_or("user");
        return Some((id.to_string(), name.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_wrong_prefix_is_unauthorized() {
        assert!(token_from_headers(None, None).is_empty());
        assert!(!token_from_headers(Some("secret"), None).starts_with(TOKEN_PREFIX));
        assert!(!token_from_headers(None, Some("Bearer secret")).starts_with(TOKEN_PREFIX));
        assert_eq!(
            token_from_headers(None, Some("Bearer summa_abc")),
            "summa_abc"
        );
        assert_eq!(
            token_from_headers(Some("summa_header"), Some("Bearer other")),
            "summa_header"
        );
        assert_eq!(token_from_headers(None, Some("summa_raw")), "summa_raw");
        assert!(token_from_headers(None, Some("Bearer summa_abc")).starts_with(TOKEN_PREFIX));
    }

    #[test]
    fn auth_failures_are_not_500() {
        assert_eq!(auth_error_status(AuthError::Unauthorized), 401);
        assert_eq!(auth_error_status(AuthError::Unavailable), 503);
    }
}
