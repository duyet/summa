use serde::Deserialize;
use worker::*;

mod analytics;
mod auth;
mod sinks;
mod types;

use analytics::{analytics_window, load_points, summarize};
use auth::{
    auth_error_response, is_owner_account, list_api_keys, mint_api_key, opt_var,
    public_error_for_worker, require_api_key, require_session, revoke_api_key,
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
        Err(e) => {
            let (status, error) = public_error_for_worker(&e.to_string());
            let res =
                Response::from_json(&serde_json::json!({ "error": error }))?.with_status(status);
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
        assert!(html.contains("href=\"#install\""));
        assert!(
            html.contains("href=\"https://burn.duyet.net\""),
            "burn.duyet.net must be a real href"
        );
        assert!(
            html.contains("The result of this project is the live spend dashboard at"),
            "hero must frame burn.duyet.net as the project result"
        );
        assert!(
            html.contains("The analytics product of that pipeline is"),
            "body must name burn as the analytics product"
        );
        assert!(
            html.contains(">analytics</a>"),
            "nav must link analytics to burn.duyet.net"
        );
        assert!(
            html.matches("href=\"https://burn.duyet.net\"").count() >= 3,
            "burn must appear as nav, hero, body, not only footer"
        );
        assert!(!html.contains("__CLERK_NOTE__"));
        assert!(!html.contains("__CLERK_PK__"));
        assert!(!html.contains("__CLERK_SCRIPT__"));
    }

    #[test]
    fn dashboard_does_not_advertise_commands_that_do_not_exist() {
        // `summa keys create` was printed as a label, but no Keys variant exists
        // in the CLI (apps/cli/src/cli.rs). A landing page that teaches a broken
        // command costs more trust than it earns.
        let html = super::dashboard_html("", "0.1.1");
        assert!(
            !html.contains("summa keys"),
            "page must not reference a `summa keys` subcommand that does not exist"
        );
    }

    #[test]
    fn minted_token_is_copyable() {
        // The token is shown exactly once and then only hashed server-side, so
        // hand-selecting a 70-char secret from `word-break: break-all` text is a
        // real failure mode. The mint result must ship its own copy control.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("shown once"), "must warn the token is one-shot");
        assert!(
            html.contains("className = \"secret\""),
            "mint result needs a block built for the token"
        );
        assert!(
            html.contains("className = \"copy\""),
            "the one-shot token block needs its own copy control, not just the install one"
        );
        assert!(
            html.contains("data-copy"),
            "copy controls carry their text via data-copy"
        );
        assert!(
            !html.contains("word-break: break-all"),
            "never break a secret across lines; wrap on the boundary instead"
        );
    }

    #[test]
    fn dashboard_exposes_key_list_and_revoke() {
        // GET /v1/keys and DELETE /v1/keys/:id already exist (lib.rs). Without
        // UI callers a tenant can mint keys forever and never see or kill one.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("/v1/keys"), "must list keys from the API");
        assert!(
            html.contains("encodeURIComponent(id)"),
            "revoke must target a specific key id"
        );
        assert!(html.contains("revoked"), "must render revoked keys, not hide them");
    }

    #[test]
    fn mint_is_guarded_against_double_submit() {
        // POST /v1/keys mints a new secret every call. A double click silently
        // burns a key that is never displayed, so the submit handler re-enters
        // through a busy latch.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("if (busy) return"), "submit must latch while in flight");
        assert!(html.contains("setBusy(false)"), "busy state must always be released");
    }

    #[test]
    fn dashboard_is_keyboard_and_motion_accessible() {
        // Regression guard: the page is dark-on-dark, so low-contrast greys and
        // missing focus rings are invisible to sighted mouse users but block
        // keyboard and low-vision users outright.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains(":focus-visible"), "interactive elements need a focus ring");
        assert!(html.contains("prefers-reduced-motion"), "motion must be opt-out");
        assert!(html.contains("--dim: #8a8a83"), "muted text must clear WCAG AA on --bg");
    }

    #[test]
    fn dashboard_has_landmarks_and_heading_order() {
        // Keyboard and screen-reader users navigate by landmark and heading
        // level; the keys panel previously had no heading at all.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("<main"), "page needs a main landmark");
        assert!(html.contains("class=\"skip\""), "needs a skip link past the nav");
        assert!(html.contains("aria-label=\"Primary\""), "nav must be named");
        assert!(html.contains("id=\"keys-h\""), "the keys panel needs a heading");
        assert!(
            html.contains("aria-labelledby=\"keys-h\""),
            "the keys section must point at its heading"
        );
    }

    #[test]
    fn copy_targets_keep_a_40px_hit_area() {
        // The one-shot secret block builds its copy control in JS, so a
        // `min-height` declared only on `.btn` left it an 18px sliver — well
        // under the 40px floor and near the 44px touch target. The rule belongs
        // on `.copy` so every copy control inherits it however it is created.
        let html = super::dashboard_html("", "0.1.1");
        let copy_rule = html
            .split(".copy {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("a .copy rule must exist");
        assert!(
            copy_rule.contains("min-height: 40px"),
            "min-height must be declared on .copy, not inherited from .btn, \
             so the JS-built secret copy control also gets a 40px target"
        );
    }

    #[test]
    fn sticky_nav_does_not_inherit_page_bottom_padding() {
        // `.topbar-in` also carried `.wrap`, whose 72px bottom padding inflated
        // the bar from 52px to 113px — measured in a real browser, invisible in
        // the diff. The bar needs its own measure with no vertical padding.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("class=\"wrap-bar topbar-in\""));
        assert!(
            !html.contains("class=\"wrap topbar-in\""),
            "the sticky bar must not reuse the page .wrap padding"
        );
    }

    #[test]
    fn primary_mint_button_is_styled_not_browser_default() {
        // A missing `btn` class silently falls back to the UA grey button, so
        // the main call to action rendered unstyled while tests still passed.
        let html = super::dashboard_html("", "0.1.1");
        assert!(
            html.contains("class=\"btn\" id=\"mint\""),
            "the mint button must carry the button class"
        );
    }

    #[test]
    fn key_names_are_rendered_as_text_not_markup() {
        // Key names are user-supplied and returned by the API. Building rows
        // with innerHTML would make the keys panel a stored-XSS sink for an
        // account that can name its own keys.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("textContent = k.name"));
        let secret_rendering = html
            .split("function showSecret")
            .nth(1)
            .expect("showSecret must exist");
        assert!(
            secret_rendering.contains("pre.textContent = snippet"),
            "the token must be set via textContent"
        );
    }

    #[test]
    fn wide_terminal_output_scrolls_instead_of_clipping() {
        // `.term { overflow: hidden }` with a non-breaking `<code>` install line
        // pushed the real command off-screen on narrow viewports.
        let html = super::dashboard_html("", "0.1.1");
        assert!(html.contains("overflow-x: auto"), "wide rows must scroll, not clip");
        assert!(html.contains("overflow-wrap: anywhere"), "long commands must wrap");
    }

    #[test]
    fn dashboard_with_clerk_injects_script_and_signin() {
        let html = super::dashboard_html("pk_test_x", "0.1.2");
        assert!(html.contains("data-clerk-publishable-key=\"pk_test_x\""));
        assert!(html.contains("clerk.session.getToken"));
        assert!(html.contains("headers.Authorization"));
        assert!(!html.contains("__CLERK_PK__"));
        let bare = super::dashboard_html("", "0.1.2");
        assert!(!bare.contains("clerk.browser.js"));
    }
}
