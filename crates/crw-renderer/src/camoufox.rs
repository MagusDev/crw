//! Camoufox stealth renderer tier (opt-in, REST — not CDP).
//!
//! Backed by a [`camofox-browser`](https://github.com/jo-inc/camofox-browser)
//! REST sidecar (default port `9377`) which drives a Camoufox (Firefox-fork)
//! browser with C++-level fingerprint spoofing. This tier covers
//! fingerprint/bot-challenge blocks (e.g. Cloudflare 403) that the CDP tiers
//! cannot pass.
//!
//! ## Why REST, not CDP
//! Camoufox does not expose a usable Chromium-CDP `Fetch.*` surface, so it is
//! reached over the sidecar's HTTP API instead of the [`crate::cdp`] path. The
//! renderer is a plain [`reqwest::Client`] — it never touches `cdp.rs`.
//!
//! ## Sequence (per request, fresh session — no cookie carry-over)
//! 1. `POST {base}/tabs` `{userId, sessionKey, url}` → `{tabId, url}` (the
//!    `url` body navigates the new tab as it is created).
//! 2. `POST {base}/tabs/{tabId}/evaluate` `{userId, expression}` with
//!    `expression = "document.documentElement.outerHTML"` → `{ok, result}`.
//!    `result` is the fully JS-rendered DOM string — exactly what CRW's
//!    post-render pipeline (`only_main_content`, tag filters, markdown) needs.
//! 3. `DELETE {base}/sessions/{userId}` — ALWAYS, even on error, so the sidecar
//!    never leaks a server-side session.
//!
//! The endpoint contract above is from the camofox-browser `openapi.json`.

use async_trait::async_trait;
use crw_core::Deadline;
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::FetchResult;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::traits::PageFetcher;

/// JS expression evaluated to retrieve the post-render DOM.
const OUTER_HTML_EXPR: &str = "document.documentElement.outerHTML";
/// Fixed budget for the best-effort session cleanup. Deliberately NOT derived
/// from the request deadline: a hung cleanup must never consume the caller's
/// remaining budget.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for the `is_available` health probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ceiling on the post-challenge settle wait when the caller gave no `waitFor`.
/// Only ever spent on a page that already tripped [`looks_like_wall`], and the
/// sidecar returns as soon as the page settles — this is a cap, not a sleep.
const DEFAULT_SETTLE_MS: u64 = 10_000;

/// Opt-in Camoufox stealth renderer. Construct via [`CamoufoxRenderer::new`].
pub struct CamoufoxRenderer {
    name: String,
    /// Trimmed base URL, no trailing slash (e.g. `http://localhost:9377`).
    base_url: String,
    /// Bearer token; empty string means no `Authorization` header is sent.
    api_key: String,
    /// Overall per-request REST budget (`config.camoufox_timeout()`).
    timeout: Duration,
    client: reqwest::Client,
}

impl CamoufoxRenderer {
    pub fn new(name: &str, base_url: &str, api_key: &str, timeout_ms: u64) -> Self {
        Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            timeout: Duration::from_millis(timeout_ms),
            client: reqwest::Client::new(),
        }
    }

    /// Attach the bearer header when an API key is configured.
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            rb
        } else {
            rb.bearer_auth(&self.api_key)
        }
    }

    /// Random opaque identifier (`<prefix>` + 32 hex chars). Fresh per request
    /// so sessions never share cookies — safer for stealth.
    fn rand_id(prefix: &str) -> String {
        let n: u128 = rand::random();
        format!("{prefix}{n:032x}")
    }

    /// Remaining budget for the next REST call: the smaller of the request
    /// deadline's remaining time and the configured camoufox timeout. This is
    /// the single source of the clamp formula — every REST call uses it.
    fn call_budget(&self, deadline: &Deadline) -> Duration {
        deadline.remaining().min(self.timeout)
    }

    /// Send a JSON POST, honoring the deadline, and parse a JSON response.
    /// Maps transport/timeout/non-2xx into the appropriate [`CrwError`].
    async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
        deadline: &Deadline,
    ) -> CrwResult<serde_json::Value> {
        let budget = self.call_budget(deadline);
        if budget.is_zero() {
            return Err(CrwError::Timeout(
                deadline.overrun().as_millis().max(1) as u64
            ));
        }
        let url = format!("{}{}", self.base_url, path);
        let fut = self.auth(self.client.post(&url).json(body)).send();
        let resp = tokio::time::timeout(budget, fut)
            .await
            .map_err(|_| CrwError::Timeout(budget.as_millis() as u64))?
            .map_err(|e| CrwError::RendererError(format!("camoufox POST {path}: {e}")))?;
        let status = resp.status();
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CrwError::RendererError(format!("camoufox POST {path} body: {e}")))?;
        if !status.is_success() {
            let msg = value
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error");
            return Err(CrwError::RendererError(format!(
                "camoufox POST {path} -> HTTP {}: {msg}",
                status.as_u16()
            )));
        }
        Ok(value)
    }

    /// Create a fresh tab navigated to `url`. Returns its `tabId`. Some sidecar
    /// builds intermittently omit `tabId` under proxy-IP races on the first
    /// attempt, so we retry exactly once before failing.
    async fn create_tab(
        &self,
        url: &str,
        user_id: &str,
        session_key: &str,
        deadline: &Deadline,
    ) -> CrwResult<String> {
        let body = serde_json::json!({
            "userId": user_id,
            "sessionKey": session_key,
            "url": url,
        });
        for attempt in 0..2u8 {
            let value = self.post_json("/tabs", &body, deadline).await?;
            if let Some(tab_id) = value.get("tabId").and_then(|t| t.as_str()) {
                return Ok(tab_id.to_string());
            }
            tracing::debug!(
                renderer = %self.name,
                attempt,
                "camoufox create tab returned no tabId, retrying"
            );
        }
        Err(CrwError::RendererError(
            "camoufox: create tab returned no tabId after retry".into(),
        ))
    }

    /// Block until the sidecar reports the page settled (`waitForPageReady`:
    /// document ready + network idle), bounded by `wait_for_ms` when the caller
    /// supplied one and by the deadline clamp always.
    ///
    /// Best-effort: a failed or timed-out wait is NOT fatal — we snapshot
    /// whatever rendered, matching the CDP tiers' partial-snapshot behavior.
    async fn wait_for_ready(
        &self,
        tab_id: &str,
        user_id: &str,
        wait_for_ms: Option<u64>,
        deadline: &Deadline,
    ) {
        let timeout_ms = wait_for_ms
            .unwrap_or(DEFAULT_SETTLE_MS)
            .min(self.call_budget(deadline).as_millis() as u64);
        if timeout_ms == 0 {
            return;
        }
        let body = serde_json::json!({ "userId": user_id, "timeout": timeout_ms });
        if let Err(e) = self
            .post_json(&format!("/tabs/{tab_id}/wait"), &body, deadline)
            .await
        {
            tracing::debug!(renderer = %self.name, error = %e, "camoufox wait failed; snapshotting anyway");
        }
    }

    /// Evaluate `document.documentElement.outerHTML` and return the DOM string.
    async fn evaluate_outer_html(
        &self,
        tab_id: &str,
        user_id: &str,
        deadline: &Deadline,
    ) -> CrwResult<String> {
        let body = serde_json::json!({
            "userId": user_id,
            "expression": OUTER_HTML_EXPR,
        });
        let value = self
            .post_json(&format!("/tabs/{tab_id}/evaluate"), &body, deadline)
            .await?;
        // Shape: { ok: bool, result: <any> }. For outerHTML, result is a string.
        if value.get("ok").and_then(|o| o.as_bool()) == Some(false) {
            return Err(CrwError::RendererError(
                "camoufox: evaluate returned ok=false".into(),
            ));
        }
        match value.get("result").and_then(|r| r.as_str()) {
            Some(html) => Ok(html.to_string()),
            None => Err(CrwError::RendererError(
                "camoufox: evaluate result was not an HTML string".into(),
            )),
        }
    }

    /// Best-effort session teardown. Uses [`CLEANUP_TIMEOUT`] (NOT the request
    /// deadline) and swallows every error — a failed cleanup is logged, never
    /// propagated.
    async fn destroy_session(&self, user_id: &str) {
        let url = format!("{}/sessions/{}", self.base_url, user_id);
        let fut = self.auth(self.client.delete(&url)).send();
        match tokio::time::timeout(CLEANUP_TIMEOUT, fut).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(
                renderer = %self.name, user_id, "camoufox session cleanup failed: {e}"
            ),
            Err(_) => tracing::warn!(
                renderer = %self.name,
                user_id,
                "camoufox session cleanup timed out after {}s (leaked server-side session)",
                CLEANUP_TIMEOUT.as_secs()
            ),
        }
    }

    /// Full create → evaluate sequence with GUARANTEED cleanup. The inner
    /// result is bound first, then `destroy_session` runs at statement level so
    /// it fires on both the Ok and Err paths, and only afterward is the result
    /// returned.
    async fn run_sequence(
        &self,
        url: &str,
        user_id: &str,
        session_key: &str,
        wait_for_ms: Option<u64>,
        deadline: &Deadline,
    ) -> CrwResult<(u16, String)> {
        let inner = self
            .run_sequence_inner(url, user_id, session_key, wait_for_ms, deadline)
            .await;
        self.destroy_session(user_id).await;
        inner
    }

    async fn run_sequence_inner(
        &self,
        url: &str,
        user_id: &str,
        session_key: &str,
        wait_for_ms: Option<u64>,
        deadline: &Deadline,
    ) -> CrwResult<(u16, String)> {
        let tab_id = self.create_tab(url, user_id, session_key, deadline).await?;
        // An explicit `waitFor` is the caller's instruction — honor it before the
        // first snapshot. Absent, snapshot immediately: the sidecar navigates with
        // `waitUntil: 'domcontentloaded'`, which is already complete for SSR pages
        // (the common case), and paying a blind settle on every request costs
        // seconds on pages whose network never idles.
        if wait_for_ms.is_some() {
            self.wait_for_ready(&tab_id, user_id, wait_for_ms, deadline)
                .await;
        }
        let mut html = self.evaluate_outer_html(&tab_id, user_id, deadline).await?;
        // A challenge caught mid-flight is not a blocked page — Cloudflare swaps
        // the DOM for the real content once its JS clears. Snapshotting straight
        // after `domcontentloaded` reliably catches that interstitial (measured:
        // g2.com returns the 2.5kB challenge shell 3/3 without this, and the full
        // 867kB page with it). Wait for the page to settle, then re-snapshot once.
        if looks_like_wall(&html).is_some() {
            tracing::debug!(renderer = %self.name, "camoufox saw a challenge; waiting for it to clear");
            self.wait_for_ready(&tab_id, user_id, wait_for_ms, deadline)
                .await;
            html = self.evaluate_outer_html(&tab_id, user_id, deadline).await?;
        }
        // A bot wall or an empty body is a failure for THIS tier — surface it as
        // a retryable RendererError so the fallback loop / breaker can react.
        if html.trim().is_empty() {
            return Err(CrwError::RendererError(
                "camoufox: evaluate returned empty HTML".into(),
            ));
        }
        if let Some(kind) = looks_like_wall(&html) {
            return Err(CrwError::RendererError(format!(
                "camoufox: bot {kind} detected in rendered HTML"
            )));
        }
        // The /evaluate path does not surface the page's HTTP status; the
        // navigation in `create_tab` already succeeded server-side, so 200.
        Ok((200, html))
    }
}

/// Heuristic detection of a bot wall / challenge interstitial in rendered HTML.
/// Returns the wall kind when matched. These markers are CRW-specific (the
/// stock camofox-browser client does not classify walls); they let the tier
/// report a retryable failure instead of returning a useless challenge page.
fn looks_like_wall(html: &str) -> Option<&'static str> {
    let h = html.to_ascii_lowercase();
    // Visible-text phrases. These only ever render on an interstitial, so they
    // stand on their own at any page size.
    const NEEDLES: &[(&str, &str)] = &[
        ("just a moment", "challenge"),
        ("verifying you are human", "challenge"),
        ("checking your browser before", "challenge"),
        ("attention required! | cloudflare", "wall"),
        ("enable javascript and cookies to continue", "wall"),
    ];
    // Script-tag markers. Cloudflare injects its JS-detection script into EVERY
    // page it fronts, not just challenges, so these cannot condemn a page on
    // their own — k-ruoka.fi returns its real 195kB homepage carrying
    // `/cdn-cgi/challenge-platform/`, while g2.com's genuine interstitial is a
    // 2.5kB shell. Size is what separates them, matching the weak-marker guard
    // `detector::looks_like_cloudflare_challenge` already uses.
    //
    // ponytail: size threshold, not content analysis. A challenge shell padded
    // past 80kB would slip through — parse the body text (as
    // `detector::looks_like_generic_bot_wall` does) if that ever shows up.
    const SCRIPT_NEEDLES: &[(&str, &str)] = &[
        ("cf-challenge", "challenge"),
        ("/cdn-cgi/challenge-platform", "challenge"),
    ];
    const INTERSTITIAL_MAX_BYTES: usize = 80_000;
    NEEDLES
        .iter()
        .chain(
            SCRIPT_NEEDLES
                .iter()
                .filter(|_| html.len() <= INTERSTITIAL_MAX_BYTES),
        )
        .find(|(needle, _)| h.contains(needle))
        .map(|(_, kind)| *kind)
}

#[async_trait]
impl PageFetcher for CamoufoxRenderer {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        wait_for_ms: Option<u64>,
        deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        if deadline.expired() {
            return Err(CrwError::Timeout(
                deadline.overrun().as_millis().max(1) as u64
            ));
        }
        let start = Instant::now();
        let user_id = Self::rand_id("crw_");
        let session_key = Self::rand_id("task_");
        let (status, html) = self
            .run_sequence(url, &user_id, &session_key, wait_for_ms, &deadline)
            .await?;

        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: status,
            html,
            content_type: None,
            raw_bytes: None,
            rendered_with: Some(self.name.clone()),
            elapsed_ms: start.elapsed().as_millis() as u64,
            warning: None,
            render_decision: None,
            credit_cost: 0,
            warnings: Vec::new(),
            truncated: false,
            deadline_exceeded: deadline.remaining().is_zero(),
            captured_responses: Vec::new(),
            // Camoufox is an HTTP sidecar (no CDP) — it never captures.
            screenshot: None,
        })
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn supports_js(&self) -> bool {
        true
    }

    /// Health probe against `GET {base}/health`. Mirrors the camofox-browser
    /// client's availability check. Cheap (~5s cap) so the FallbackRenderer can
    /// skip a down sidecar instead of paying the full per-request budget.
    async fn is_available(&self) -> bool {
        let url = format!("{}/health", self.base_url);
        let fut = self.auth(self.client.get(&url)).send();
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, fut).await,
            Ok(Ok(r)) if r.status().is_success()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn renderer(base_url: &str) -> CamoufoxRenderer {
        CamoufoxRenderer::new("camoufox", base_url, "", 30_000)
    }

    fn deadline() -> Deadline {
        Deadline::from_request_ms(30_000)
    }

    async fn mount_delete_session(server: &MockServer) {
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/sessions/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "closed": 1
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn happy_path_returns_html_and_cleans_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tabId": "t1", "url": "https://example.com"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": "<html><body>real content well over the empty threshold here</body></html>"
            })))
            .mount(&server)
            .await;
        // Expect cleanup to fire exactly once.
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/sessions/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let r = renderer(&server.uri());
        let res = r
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect("fetch should succeed");
        assert_eq!(res.rendered_with.as_deref(), Some("camoufox"));
        assert_eq!(res.status_code, 200);
        assert!(res.html.contains("real content"));
        // server drop verifies the expect(1) on the DELETE mock
    }

    /// A challenge caught mid-flight must be waited out and re-snapshotted, not
    /// reported as a block. Without this the tier fails every Cloudflare-fronted
    /// page it would otherwise win (measured on g2.com, 3/3).
    #[tokio::test]
    async fn challenge_is_waited_out_then_resnapshotted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tabId": "t1", "url": "https://example.com"
            })))
            .mount(&server)
            .await;
        // The settle wait must fire exactly once — only on the challenge path.
        Mock::given(method("POST"))
            .and(path("/tabs/t1/wait"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "ready": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        // First evaluate returns the interstitial, second the cleared page.
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": "<html><head><script src=\"/cdn-cgi/challenge-platform/x.js\"></script></head></html>"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": "<html><body>the real page well past the empty threshold</body></html>"
            })))
            .mount(&server)
            .await;
        mount_delete_session(&server).await;

        let r = renderer(&server.uri());
        let res = r
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect("a cleared challenge must not fail the fetch");
        assert!(res.html.contains("the real page"));
    }

    /// A challenge that never clears still fails — the retry must not paper over
    /// a genuine block by returning the interstitial as content.
    #[tokio::test]
    async fn challenge_that_never_clears_still_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tabId": "t1", "url": "https://example.com"
            })))
            .mount(&server)
            .await;
        // A failing wait must not abort the sequence — it is best-effort.
        Mock::given(method("POST"))
            .and(path("/tabs/t1/wait"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": "<html><body>Just a moment... verifying you are human</body></html>"
            })))
            .mount(&server)
            .await;
        mount_delete_session(&server).await;

        let r = renderer(&server.uri());
        let err = r
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("an unresolved challenge must still be a failure");
        assert!(err.to_string().contains("bot challenge detected"));
    }

    /// No challenge, no `waitFor` → the settle wait must never fire. This is what
    /// keeps the common (SSR) path at ~2.7s instead of burning the full timeout
    /// on pages whose network never idles.
    #[tokio::test]
    async fn clean_page_never_waits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tabId": "t1", "url": "https://example.com"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/wait"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": "<html><body>clean page well past the empty threshold here</body></html>"
            })))
            .mount(&server)
            .await;
        mount_delete_session(&server).await;

        let r = renderer(&server.uri());
        r.fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect("fetch should succeed");
        // server drop verifies the expect(0) on the wait mock
    }

    #[tokio::test]
    async fn cleanup_fires_on_evaluate_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tabId": "t1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(
                ResponseTemplate::new(500).set_body_json(serde_json::json!({"error": "boom"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/sessions/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1) // cleanup MUST fire even though evaluate failed
            .mount(&server)
            .await;

        let r = renderer(&server.uri());
        let err = r
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("evaluate 500 should error");
        assert!(matches!(err, CrwError::RendererError(_)));
    }

    /// Helper: mock create-tab OK + evaluate returning `eval_body`, with a
    /// cleanup mock that MUST fire once. Returns the started server.
    async fn server_with_evaluate(eval_body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tabId": "t1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(eval_body))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/sessions/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1) // cleanup must fire on every error path below
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn empty_html_errors_and_cleans_up() {
        let server = server_with_evaluate(serde_json::json!({"ok": true, "result": "   "})).await;
        let err = renderer(&server.uri())
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("empty HTML should error");
        match err {
            CrwError::RendererError(m) => assert!(m.contains("empty HTML")),
            other => panic!("expected RendererError, got {other:?}"),
        }
        // server drop verifies cleanup expect(1)
    }

    #[tokio::test]
    async fn evaluate_ok_false_errors_and_cleans_up() {
        let server = server_with_evaluate(serde_json::json!({"ok": false})).await;
        let err = renderer(&server.uri())
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("ok=false should error");
        match err {
            CrwError::RendererError(m) => assert!(m.contains("ok=false")),
            other => panic!("expected RendererError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn evaluate_non_string_result_errors_and_cleans_up() {
        // result is a number, not the expected HTML string.
        let server = server_with_evaluate(serde_json::json!({"ok": true, "result": 42})).await;
        let err = renderer(&server.uri())
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("non-string result should error");
        match err {
            CrwError::RendererError(m) => assert!(m.contains("not an HTML string")),
            other => panic!("expected RendererError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_tab_id_retries_once_then_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(2) // first attempt + one retry
            .mount(&server)
            .await;
        mount_delete_session(&server).await;

        let r = renderer(&server.uri());
        let err = r
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("missing tabId should error");
        match err {
            CrwError::RendererError(m) => assert!(m.contains("no tabId")),
            other => panic!("expected RendererError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wall_detection_returns_retryable_renderer_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tabId": "t1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": "<html><head><title>Just a moment...</title></head><body>verifying you are human</body></html>"
            })))
            .mount(&server)
            .await;
        mount_delete_session(&server).await;

        let r = renderer(&server.uri());
        let err = r
            .fetch("https://example.com", &HashMap::new(), None, deadline())
            .await
            .expect_err("wall should error");
        match err {
            CrwError::RendererError(m) => assert!(m.contains("challenge")),
            other => panic!("expected RendererError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expired_deadline_short_circuits_without_http() {
        // No mocks mounted: if any HTTP call were made it would 404 and the
        // error message would differ from a Timeout.
        let server = MockServer::start().await;
        let r = renderer(&server.uri());
        let err = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                None,
                Deadline::from_request_ms(0),
            )
            .await
            .expect_err("expired deadline should error immediately");
        assert!(matches!(err, CrwError::Timeout(_)));
    }

    #[tokio::test]
    async fn is_available_true_when_health_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;
        assert!(renderer(&server.uri()).is_available().await);
    }

    #[tokio::test]
    async fn is_available_false_when_health_503() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(serde_json::json!({"ok": false})),
            )
            .mount(&server)
            .await;
        assert!(!renderer(&server.uri()).is_available().await);
    }

    #[test]
    fn wall_needles_match_and_clean_html_passes() {
        assert_eq!(looks_like_wall("Just a moment..."), Some("challenge"));
        assert_eq!(
            looks_like_wall("<script src=/cdn-cgi/challenge-platform/x>"),
            Some("challenge")
        );
        assert_eq!(
            looks_like_wall("<div class=cf-challenge>"),
            Some("challenge")
        );
        assert_eq!(
            looks_like_wall("Verifying you are human"),
            Some("challenge")
        );
        assert_eq!(
            looks_like_wall("Checking your browser before access"),
            Some("challenge")
        );
        assert_eq!(
            looks_like_wall("<h1>Attention Required! | Cloudflare</h1>"),
            Some("wall")
        );
        assert_eq!(
            looks_like_wall("Please enable JavaScript and cookies to continue"),
            Some("wall")
        );
        // A full-size page carrying Cloudflare's JS-detection script is real
        // content, not an interstitial. Measured: k-ruoka.fi's homepage renders
        // 195kB with `/cdn-cgi/challenge-platform/` in a <script src>, while
        // g2.com's genuine challenge shell is 2.5kB.
        let real_page = format!(
            "<html><body><script src=/cdn-cgi/challenge-platform/x></script>{}</body></html>",
            "real article text ".repeat(6000)
        );
        assert!(real_page.len() > 80_000);
        assert_eq!(looks_like_wall(&real_page), None);
        // ...but a visible-text phrase still condemns a page of any size.
        let big_interstitial = format!("<html><body>Just a moment...{}</body></html>", "x".repeat(90_000));
        assert_eq!(looks_like_wall(&big_interstitial), Some("challenge"));
        assert_eq!(
            looks_like_wall("<html><body>normal page</body></html>"),
            None
        );
    }

    #[tokio::test]
    async fn cleanup_uses_fixed_timeout_not_request_deadline() {
        // A near-exhausted request deadline must NOT prevent cleanup from
        // running: cleanup has its own fixed budget. We give a tiny deadline so
        // the fetch fails fast (budget exhausted mid-sequence), then assert the
        // DELETE cleanup still fired exactly once.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(
                // Large delay (2s) vs the 5ms deadline gives a ~400x margin, so
                // the create-tab call deterministically times out before its
                // response arrives even under heavy CI load — no flakiness.
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(2))
                    .set_body_json(serde_json::json!({"tabId": "t1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/sessions/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let r = renderer(&server.uri());
        // 5ms deadline << 2s create-tab delay -> create_tab times out, but
        // cleanup (fixed 5s budget) must still fire.
        let err = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                None,
                Deadline::from_request_ms(5),
            )
            .await
            .expect_err("tiny deadline should error");
        assert!(matches!(err, CrwError::Timeout(_)));
        // server drop verifies cleanup expect(1) despite the exhausted deadline
    }
}
