use serde::Deserialize;
use worker::*;

mod analytics;
mod auth;
mod sinks;
mod types;

use analytics::{analytics_window, load_points, summarize};
use auth::{
    auth_error_response, is_owner_account, list_api_keys, mint_api_key, opt_var, require_api_key,
    require_session, revoke_api_key,
};
use sinks::{collect_pings, fanout_write};
use types::{
    ch_now, cors_allow_origin, ingest_body_too_large, ingest_status_code, ping_ok,
    stamp_ingest_identity, IngestParseError, VERSION,
};

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let origin = req.headers().get("Origin").ok().flatten();
    let method = req.method();
    if method == Method::Options {
        return apply_cors(origin.as_deref(), true, Response::empty()?.with_status(204));
    }
    let public = matches!(req.path().as_str(), "/" | "/health" | "/install.sh");
    match route(req, env).await {
        Ok(res) => apply_cors(origin.as_deref(), public, res),
        Err(_) => {
            let res = Response::from_json(&serde_json::json!({"error": "internal error"}))?
                .with_status(500);
            apply_cors(origin.as_deref(), public, res)
        }
    }
}

async fn route(req: Request, env: Env) -> Result<Response> {
    let path = req.path();
    let method = req.method();
    match (method, path.as_str()) {
        (Method::Get, "/") => {
            let html = dashboard_html(&opt_var(&env, "CLERK_PUBLISHABLE_KEY"), VERSION);
            Response::from_html(html)
        }
        (Method::Get, "/install.sh") => install_sh_response(),
        (Method::Get, "/health") => Response::from_json(&serde_json::json!({
            "ok": true,
            "service": "summa",
            "version": VERSION,
        })),
        (Method::Get, "/ping") => ping(req, env).await,
        (Method::Get, "/status") => status(req, env).await,
        (Method::Post, "/v1/ingest") => ingest(req, env).await,
        (Method::Get, "/v1/analytics") => analytics(req, env, false).await,
        (Method::Get, "/v1/analytics/summary") => analytics(req, env, true).await,
        (Method::Post, "/v1/keys") => create_key(req, env).await,
        (Method::Get, "/v1/keys") => list_keys(req, env).await,
        (Method::Delete, p) if p.starts_with("/v1/keys/") => {
            let id = p.trim_start_matches("/v1/keys/");
            delete_key(req, env, id).await
        }
        _ => Response::from_json(&serde_json::json!({"error": "not found"}))
            .map(|r| r.with_status(404)),
    }
}

async fn ping(req: Request, env: Env) -> Result<Response> {
    if let Err(e) = require_api_key(&req, &env).await {
        return auth_error_response(e);
    }
    let samples = collect_pings(&env).await;
    Response::from_json(&serde_json::json!({
        "ok": ping_ok(&samples),
        "samples": samples,
    }))
}

async fn status(req: Request, env: Env) -> Result<Response> {
    let auth = match require_api_key(&req, &env).await {
        Ok(a) => a,
        Err(e) => return auth_error_response(e),
    };
    let samples = collect_pings(&env).await;
    Response::from_json(&serde_json::json!({
        "ok": ping_ok(&samples),
        "account_id": auth.account_id,
        "api_key_id": auth.api_key_id,
        "ping": samples,
    }))
}

async fn ingest(mut req: Request, env: Env) -> Result<Response> {
    let auth = match require_api_key(&req, &env).await {
        Ok(a) => a,
        Err(e) => return auth_error_response(e),
    };
    let len = req.headers().get("content-length").ok().flatten();
    if ingest_body_too_large(len.as_deref()) {
        return Response::from_json(&serde_json::json!({
            "error": "payload too large",
            "max_bytes": types::MAX_INGEST_BYTES,
        }))
        .map(|r| r.with_status(413));
    }
    let bytes = match req.bytes().await {
        Ok(b) => b,
        Err(_) => {
            return Response::from_json(&serde_json::json!({"error": "invalid body"}))
                .map(|r| r.with_status(400));
        }
    };
    let parsed = match types::parse_ingest_bytes(&bytes) {
        Ok(p) => p,
        Err(e) => {
            let mut body = serde_json::json!({ "error": e.message() });
            if e == IngestParseError::TooLarge {
                body["max_bytes"] = types::MAX_INGEST_BYTES.into();
            }
            if e == IngestParseError::TooManyEvents {
                body["max"] = types::MAX_INGEST_EVENTS.into();
            }
            return Response::from_json(&body).map(|r| r.with_status(e.status()));
        }
    };
    let now = ch_now();
    let mut events = parsed.events;
    for e in &mut events {
        stamp_ingest_identity(e, &auth.account_id, &auth.api_key_id, &now);
    }
    let sinks = fanout_write(&env, &events).await;
    let code = ingest_status_code(&sinks);
    let res = Response::from_json(&serde_json::json!({
        "accepted": events.len(),
        "rejected": parsed.rejected,
        "sinks": sinks,
    }))?;
    Ok(res.with_status(code))
}

async fn analytics(req: Request, env: Env, summary: bool) -> Result<Response> {
    let auth = match require_api_key(&req, &env).await {
        Ok(a) => a,
        Err(e) => return auth_error_response(e),
    };
    let url = req.url()?;
    let group = url
        .query_pairs()
        .find(|(k, _)| k == "group")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| "source".into());
    let days = url
        .query_pairs()
        .find(|(k, _)| k == "days")
        .and_then(|(_, v)| v.parse::<i64>().ok());
    let since = url
        .query_pairs()
        .find(|(k, _)| k == "since")
        .map(|(_, v)| v.into_owned());
    let until = url
        .query_pairs()
        .find(|(k, _)| k == "until")
        .map(|(_, v)| v.into_owned());
    let default_days = if summary {
        Some(days.unwrap_or(7))
    } else {
        days
    };
    let (since, until) = match analytics_window(since.as_deref(), until.as_deref(), default_days) {
        Ok(w) => w,
        Err(e) => {
            return Response::from_json(&serde_json::json!({"error": e}))
                .map(|r| r.with_status(400));
        }
    };
    let include_legacy = is_owner_account(&env, &auth.account_id)
        .await
        .unwrap_or(false);
    let points = match load_points(
        &env,
        &auth.account_id,
        include_legacy,
        &group,
        &since,
        &until,
    )
    .await
    {
        Ok(p) => p,
        Err(_) => {
            return Response::from_json(&serde_json::json!({"error": "analytics unavailable"}))
                .map(|r| r.with_status(502));
        }
    };
    if summary {
        return Response::from_json(&summarize(&since, &until, &points));
    }
    Response::from_json(&serde_json::json!({
        "since": since,
        "until": until,
        "group": group,
        "points": points,
    }))
}

#[derive(Deserialize, Default)]
struct KeyName {
    #[serde(default)]
    name: String,
}

async fn create_key(mut req: Request, env: Env) -> Result<Response> {
    let auth = match require_session(&req, &env).await {
        Ok(a) => a,
        Err(e) => return auth_error_response(e),
    };
    let name = req.json::<KeyName>().await.unwrap_or_default().name;
    let (id, token, prefix) = mint_api_key(&env, &auth.account_id, &name).await?;
    Response::from_json(&serde_json::json!({ "id": id, "token": token, "prefix": prefix }))
}

async fn list_keys(req: Request, env: Env) -> Result<Response> {
    let auth = match require_session(&req, &env).await {
        Ok(a) => a,
        Err(e) => return auth_error_response(e),
    };
    let keys = list_api_keys(&env, &auth.account_id).await?;
    Response::from_json(&serde_json::json!({
        "account_id": auth.account_id,
        "keys": keys,
    }))
}

async fn delete_key(req: Request, env: Env, id: &str) -> Result<Response> {
    let auth = match require_session(&req, &env).await {
        Ok(a) => a,
        Err(e) => return auth_error_response(e),
    };
    let ok = revoke_api_key(&env, &auth.account_id, id).await?;
    if !ok {
        return Response::from_json(&serde_json::json!({"error": "not found"}))
            .map(|r| r.with_status(404));
    }
    Response::from_json(&serde_json::json!({"ok": true, "id": id, "revoked": true}))
}

pub fn install_script() -> &'static str {
    include_str!("../../../install.sh")
}

fn install_sh_response() -> Result<Response> {
    let headers = Headers::new();
    let _ = headers.set("content-type", "text/plain; charset=utf-8");
    let _ = headers.set("content-disposition", "inline; filename=\"install.sh\"");
    let _ = headers.set("cache-control", "public, max-age=300");
    Ok(Response::ok(install_script())?.with_headers(headers))
}

fn apply_cors(origin: Option<&str>, public: bool, res: Response) -> Result<Response> {
    let allow = cors_allow_origin(origin).or_else(|| if public { Some("*".into()) } else { None });
    let Some(allow) = allow else {
        return Ok(res);
    };
    let headers = res.headers().clone();
    let _ = headers.set("access-control-allow-origin", &allow);
    let _ = headers.set(
        "access-control-allow-headers",
        "Authorization, Content-Type, X-Summa-Token",
    );
    let _ = headers.set("access-control-allow-methods", "GET, POST, DELETE, OPTIONS");
    let _ = headers.set("vary", "Origin");
    Ok(res.with_headers(headers))
}

fn dashboard_html(publishable_key: &str, version: &str) -> String {
    let clerk_script = if publishable_key.is_empty() {
        ""
    } else {
        &format!(
            "<script async crossorigin=\"anonymous\" data-clerk-publishable-key=\"{pk}\" src=\"https://cdn.jsdelivr.net/npm/@clerk/clerk-js@5/dist/clerk.browser.js\"></script>",
            pk = publishable_key
        )
    };
    let clerk_note = if publishable_key.is_empty() {
        "clerk is not configured on this deployment; minting needs a session or bootstrap token."
    } else {
        "sign in to mint a telemetry_token for ~/.config/summa/credentials.toml."
    };
    include_str!("dashboard.html")
        .replace("__VERSION__", version)
        .replace("__CLERK_NOTE__", clerk_note)
        .replace("__CLERK_SCRIPT__", &clerk_script)
        .replace("__CLERK_PK__", publishable_key)
}

#[cfg(test)]
mod tests {
    use super::install_script;

    #[test]
    fn install_script_is_curl_bash() {
        let s = install_script();
        assert!(s.contains("summa installer"));
        assert!(s.contains("SUMMA_DOWNLOAD_BASE"));
        assert!(s.contains("beta"));
        assert!(s.contains("SUMMA_CHANNEL"));
        assert!(!s.contains("nightly"));
        assert!(s.contains("curl -fsSL"));
    }

    #[test]
    fn dashboard_is_terminal_landing() {
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("curl -fsSL https://summa.duyet.net/install.sh | bash"));
        assert!(html.contains("class=\"term\""));
        assert!(html.contains("v0.1.1"));
        // Demo terminal mirrors real output: summa — machine: …
        assert!(html.contains("=== Summary ==="));
        assert!(html.contains("sink summa-cloud: 341 rows"));
        // Menu: install is an in-page anchor; burn linked only from footer.
        assert!(html.contains("href=\"#install\""));
        assert_eq!(
            html.matches("https://burn.duyet.net").count(),
            1,
            "burn only linked from the footer"
        );
        assert!(!html.contains("__CLERK_NOTE__"));
        assert!(!html.contains("__CLERK_PK__"));
        assert!(!html.contains("__CLERK_SCRIPT__"));
    }

    #[test]
    fn dashboard_with_clerk_injects_script_and_signin() {
        let html = super::dashboard_html("pk_test_x", "0.1.2");
        assert!(html.contains("data-clerk-publishable-key=\"pk_test_x\""));
        assert!(!html.contains("__CLERK_PK__"));
        let bare = super::dashboard_html("", "0.1.2");
        assert!(!bare.contains("clerk.browser.js"));
    }
}
