//! `host_http.get` and `host_http.post`.
//!
//! One step type per HTTP method, so a tool that only fetches is provably
//! read-only in its manifest; further methods get step types when something
//! needs them. Both share one implementation — a redirect may lawfully
//! rewrite the method mid-chain (a 303, or a legacy 302 on a POST), so the
//! method is fixed per *step type* but still varies per hop.

use std::sync::OnceLock;
use std::time::Duration;

use gwead::futures::{StreamExt as _, TryStreamExt};
use gwead::indexmap::IndexMap;
use gwead::kernel::streams::{ReadableSource, lock_shared};
use gwead::kernel::{PluginExecution, StepError, StepOutput};
use gwead::serde_json::{Map, Value, json};
use reqwest::Method;
use tokio::time::Instant;
use url::Url;

use super::{
    StepFuture, bool_param, cancelled, capped, lossy_capped, resolve, str_param, u64_param,
};
use crate::host::{approval, approve};
use crate::operator::Access;

/// Default cap on a buffered (non-streaming) response body.
pub const DEFAULT_MAX_BODY_BYTES: u64 = 8 << 20;
/// Hard ceiling on `max_bytes`: larger requests are clamped, so a plugin
/// cannot ask the host to buffer without bound.
pub const BODY_BYTES_CEILING: u64 = 64 << 20;
/// Default budget for reaching a response — the whole redirect chain, and
/// the body too when it is buffered.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Hard ceiling on `timeout_ms` and `idle_timeout_ms`: larger requests are
/// clamped, so neither budget can be voided by asking for forever.
pub const TIMEOUT_MS_CEILING: u64 = 3_600_000;
/// Default limit on the gap between chunks of a streamed body.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 120_000;
/// Default limit on redirects followed in one request.
pub const DEFAULT_MAX_REDIRECTS: u64 = 5;

/// Time allowed to establish a connection, inside the overall budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("gwennol/", env!("CARGO_PKG_VERSION")))
            // Redirects are followed by this step body, not by reqwest: a
            // hop the client takes on its own reaches a host no gate ever
            // saw. See `redirect_target`.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client with static config")
    })
}

/// Where a 3xx sends the request next.
#[derive(Debug, PartialEq, Eq)]
struct Redirect {
    /// Absolute URL of the next hop.
    url: Url,
    /// Method to use for it.
    method: Method,
    /// Whether the body is dropped (a method rewrite; RFC 9110 §15.4).
    drop_body: bool,
    /// Whether the hop leaves the current origin, so the plugin's headers
    /// must not go with it.
    cross_origin: bool,
}

/// Decide what a response means for redirection.
///
/// `Ok(None)` is "this is the answer" — a non-redirect status, or a 3xx with
/// no `Location`, which is a response in its own right and not a hop.
///
/// A redirect that would leave `http`/`https`, or drop an `https` request to
/// cleartext `http`, is refused rather than followed: the plugin asked for a
/// protected channel and a redirect is the far end's word, not the
/// operator's.
fn redirect_target(
    from: &Url,
    status: u16,
    location: Option<&str>,
    method: &Method,
) -> Result<Option<Redirect>, String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = location.map(str::trim).filter(|l| !l.is_empty()) else {
        return Ok(None);
    };
    let url = from
        .join(location)
        .map_err(|e| format!("{status} redirect to an unparseable location '{location}': {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "refusing a {status} redirect to a '{}' URL",
            url.scheme()
        ));
    }
    if from.scheme() == "https" && url.scheme() == "http" {
        return Err(format!(
            "refusing a {status} redirect from https to cleartext http ({})",
            safe_url(&url)
        ));
    }
    // 303 always becomes GET; 301 and 302 do for anything but GET/HEAD,
    // which is what every client does and what servers expect.
    let rewrite = status == 303
        || (matches!(status, 301 | 302) && !matches!(*method, Method::GET | Method::HEAD));
    Ok(Some(Redirect {
        cross_origin: from.origin() != url.origin(),
        method: if rewrite { Method::GET } else { method.clone() },
        drop_body: rewrite,
        url,
    }))
}

/// Wrap a response body so a stalled peer ends the stream instead of
/// holding it open, and so cancelling the invocation tears it down.
///
/// The idle limit is per chunk: it catches a peer that stops talking, which
/// is the failure a long-lived SSE stream actually has. A total budget would
/// be wrong here — a model streaming a long answer is working, not stuck.
fn guarded_body(
    source: ReadableSource,
    idle: Duration,
    cancel: gwead::tokio_util::sync::CancellationToken,
) -> ReadableSource {
    Box::pin(gwead::futures::stream::unfold(Some(source), move |state| {
        let cancel = cancel.clone();
        async move {
            let mut source = state?;
            let next = tokio::select! {
                () = cancel.cancelled() => {
                    return Some((Err(std::io::Error::other("cancelled")), None));
                }
                r = tokio::time::timeout(idle, source.next()) => r,
            };
            match next {
                Ok(Some(item)) => {
                    let keep = item.is_ok().then_some(source);
                    Some((item, keep))
                }
                Ok(None) => None,
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("no response data for {idle:?}"),
                    )),
                    None,
                )),
            }
        }
    }))
}

/// Strip everything from a URL that can carry a credential — userinfo,
/// query, fragment — leaving scheme, host and path. Every URL headed for
/// a plugin-visible string or a log goes through here.
pub(crate) fn scrub(u: &mut Url) {
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_query(None);
    u.set_fragment(None);
}

/// reqwest's `Display` appends `for url (…)` with the URL unredacted, so
/// a send failure would carry the query string into plugin-visible
/// strings and logs. Scrub the error's own copy of the URL before
/// formatting it.
fn scrubbed(mut e: reqwest::Error) -> reqwest::Error {
    if let Some(u) = e.url_mut() {
        scrub(u);
    }
    e
}

/// A URL rendered safe for error messages, via [`scrub`].
fn safe_url(url: &Url) -> String {
    let mut u = url.clone();
    scrub(&mut u);
    u.to_string()
}

fn host_of(url: &Url) -> Result<String, StepError> {
    url.host_str()
        .map(str::to_string)
        .ok_or_else(|| StepError::Failed(format!("URL '{}' has no host", safe_url(url))))
}

/// `host_http.get`:
/// `{url, headers?, stream?, max_bytes?, timeout_ms?, idle_timeout_ms?,
/// max_redirects?}`.
///
/// See [`http_post`] for the shared semantics; a GET carries no body, and
/// one supplied anyway is refused.
pub fn http_get<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    request(ex, params, Method::GET)
}

/// `host_http.post`:
/// `{url, headers?, body?, stream?, max_bytes?, timeout_ms?,
/// idle_timeout_ms?, max_redirects?}`.
///
/// Buffered (default): result `{status, body}` where `body` is the decoded
/// text (capped at `max_bytes`, see `truncated`). Streaming
/// (`stream: true`): result `{status, body}` where `body` is a readable
/// stream handle the plugin drains through the streams ABI — the shape a
/// model provider needs for server-sent events. Either way the sidecar
/// metadata carries `status`, `headers`, and the `url` finally answered.
///
/// `body` may be a string (sent as-is) or any other JSON value (serialised,
/// with `content-type: application/json` unless a header overrides it).
///
/// # Redirects are hops, and every hop is gated
///
/// The client follows none by itself. Each `Location` is resolved here and
/// run through both gates again — the kernel's `network:egress:<host>`
/// grant first, then the operator, shown the concrete next URL — because a
/// followed redirect is a request to a host the plugin never declared and
/// the operator never saw. A hop that leaves the origin carries *none* of
/// the plugin's headers: which of them holds authority is the plugin's
/// business (`x-api-key` is authority to Anthropic), so the host assumes
/// they all do — a redirect is the far end choosing where a plugin's
/// credential goes, which is not its choice to make.
///
/// # Time
///
/// `timeout_ms` bounds reaching a response, redirect chain included, and
/// the body as well when it is buffered. A streamed body is bounded instead
/// by `idle_timeout_ms` between chunks, and by the invocation's cancel
/// token.
pub fn http_post<'a>(
    ex: &'a mut (dyn PluginExecution + Send),
    params: &'a Value,
) -> StepFuture<'a> {
    request(ex, params, Method::POST)
}

/// Shared implementation. `initial` is the calling step type's fixed method
/// for the first hop; a redirect may still rewrite it mid-chain.
fn request<'a>(
    ex: &'a mut (dyn PluginExecution + Send),
    params: &'a Value,
    initial: Method,
) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let url_str = str_param(&p, "url")?;
        let mut url = Url::parse(url_str)
            .map_err(|e| StepError::Failed(format!("param 'url' is not a valid URL: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(StepError::Failed(format!(
                "param 'url' must be http or https, got '{}'",
                url.scheme()
            )));
        }
        let mut method = initial;
        let mut headers: Vec<(String, String)> = match p.get("headers") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    _ => Err(StepError::Failed(format!("header '{k}' must be a string"))),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(StepError::Failed(
                    "param 'headers' must be an object".into(),
                ));
            }
        };
        let mut body = match p.get("body") {
            None | Some(Value::Null) => None,
            Some(_) if method == Method::GET => {
                return Err(StepError::Failed(
                    "host_http.get sends no body; use host_http.post".into(),
                ));
            }
            other => other.cloned(),
        };
        let stream = bool_param(&p, "stream", false)?;
        let max = capped(
            u64_param(&p, "max_bytes", DEFAULT_MAX_BODY_BYTES)?,
            BODY_BYTES_CEILING,
        );
        let timeout = Duration::from_millis(
            u64_param(&p, "timeout_ms", DEFAULT_TIMEOUT_MS)?.min(TIMEOUT_MS_CEILING),
        );
        let idle = Duration::from_millis(
            u64_param(&p, "idle_timeout_ms", DEFAULT_IDLE_TIMEOUT_MS)?.min(TIMEOUT_MS_CEILING),
        );
        let max_redirects = u64_param(&p, "max_redirects", DEFAULT_MAX_REDIRECTS)?;

        // Set after the first approval, so however long the operator
        // deliberates is not billed to the network budget. Mid-chain
        // approvals do run on the clock: by then the network is in play.
        let mut deadline = None;
        let cancel = ex.cancel_token();
        let mut hops = 0u64;

        let resp = loop {
            let host = host_of(&url)?;

            // Manifest first, operator second — for this hop, not just the
            // one the plugin named.
            ex.check_network_egress(&host).map_err(StepError::Failed)?;
            let ask = approval(
                &*ex,
                Access::Http {
                    method: method.to_string(),
                    url: url.to_string(),
                },
            );
            approve(ask).await?;
            let hop_deadline = *deadline.get_or_insert_with(|| Instant::now() + timeout);

            let mut req = client().request(method.clone(), url.clone());
            let mut has_content_type = false;
            for (k, v) in &headers {
                has_content_type |= k.eq_ignore_ascii_case("content-type");
                req = req.header(k, v);
            }
            match &body {
                None => {}
                Some(Value::String(s)) => req = req.body(s.clone()),
                Some(other) => {
                    if !has_content_type {
                        req = req.header("content-type", "application/json");
                    }
                    req = req.body(other.to_string());
                }
            }

            let resp = tokio::select! {
                r = tokio::time::timeout_at(hop_deadline, req.send()) => match r {
                    Ok(r) => r.map_err(|e| {
                        StepError::Failed(format!("request to {host}: {}", scrubbed(e)))
                    })?,
                    Err(_) => return Err(StepError::Failed(format!(
                        "request to {host} exceeded timeout of {timeout:?}"
                    ))),
                },
                () = cancel.cancelled() => return Err(cancelled()),
            };

            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let next = redirect_target(&url, resp.status().as_u16(), location.as_deref(), &method)
                .map_err(StepError::Failed)?;
            let Some(next) = next else { break resp };

            if hops >= max_redirects {
                return Err(StepError::Failed(format!(
                    "more than {max_redirects} redirects following {}",
                    safe_url(&url)
                )));
            }
            hops += 1;
            if next.cross_origin {
                // All of them, not a known-credential list: which header
                // carries authority is the plugin's business, so the host
                // assumes every one does.
                headers.clear();
            }
            if next.drop_body {
                body = None;
            }
            method = next.method;
            url = next.url;
        };

        let status = resp.status().as_u16();
        let mut hdrs = Map::new();
        for (k, v) in resp.headers() {
            hdrs.insert(
                k.as_str().to_string(),
                Value::String(String::from_utf8_lossy(v.as_bytes()).into_owned()),
            );
        }
        let mut metadata = IndexMap::new();
        metadata.insert("status".to_string(), json!(status));
        metadata.insert("headers".to_string(), Value::Object(hdrs));
        metadata.insert("url".to_string(), json!(url.to_string()));

        if stream {
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let source = resp
                .bytes_stream()
                .map_err(|e| std::io::Error::other(scrubbed(e)))
                .boxed();
            let handle = lock_shared(ex.streams())
                .register_readable(content_type, guarded_body(source, idle, cancel));
            return Ok(StepOutput::with_metadata(
                json!({"status": status, "body": handle.get()}),
                metadata,
            ));
        }

        let host = host_of(&url)?;
        let deadline = deadline.expect("set when the first hop was approved");
        let mut chunks = resp.bytes_stream();
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let chunk = tokio::select! {
                r = tokio::time::timeout_at(deadline, chunks.next()) => match r {
                    Ok(c) => c,
                    Err(_) => return Err(StepError::Failed(format!(
                        "reading response from {host} exceeded timeout of {timeout:?}"
                    ))),
                },
                () = cancel.cancelled() => return Err(cancelled()),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|e| {
                StepError::Failed(format!("reading response from {host}: {}", scrubbed(e)))
            })?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() > max {
                // Already past the cap: the rest of the body stays unread,
                // so max_bytes bounds host memory, not just the result.
                break;
            }
        }
        let (body, truncated) = lossy_capped(&bytes, max);
        Ok(StepOutput::with_metadata(
            json!({"status": status, "body": body, "truncated": truncated}),
            metadata,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from() -> Url {
        Url::parse("https://api.example.com/v1/messages").unwrap()
    }

    fn target(status: u16, location: &str, method: Method) -> Result<Option<Redirect>, String> {
        redirect_target(&from(), status, Some(location), &method)
    }

    #[test]
    fn non_redirect_status_is_the_answer() {
        assert_eq!(target(200, "/elsewhere", Method::POST), Ok(None));
        assert_eq!(
            redirect_target(&from(), 302, None, &Method::POST),
            Ok(None),
            "a 3xx without a Location is a response, not a hop"
        );
    }

    #[test]
    fn relative_locations_resolve_against_the_current_url() {
        let r = target(307, "/v2/messages", Method::POST).unwrap().unwrap();
        assert_eq!(r.url.as_str(), "https://api.example.com/v2/messages");
        assert!(!r.cross_origin);
        assert_eq!(r.method, Method::POST, "307 preserves the method");
        assert!(!r.drop_body);
    }

    #[test]
    fn see_other_and_legacy_post_redirects_become_get_without_a_body() {
        for status in [301, 302, 303] {
            let r = target(status, "/done", Method::POST).unwrap().unwrap();
            assert_eq!(r.method, Method::GET, "{status}");
            assert!(r.drop_body, "{status}");
        }
        let r = target(302, "/done", Method::GET).unwrap().unwrap();
        assert_eq!(r.method, Method::GET);
        assert!(!r.drop_body, "a GET keeps its (absent) body");
    }

    #[test]
    fn another_origin_is_flagged_including_a_bare_port_change() {
        let r = target(302, "https://evil.example.net/x", Method::GET)
            .unwrap()
            .unwrap();
        assert!(r.cross_origin);
        let r = target(302, "https://api.example.com:8443/x", Method::GET)
            .unwrap()
            .unwrap();
        assert!(r.cross_origin, "a different port is a different origin");
        let r = target(302, "https://api.example.com:443/x", Method::GET)
            .unwrap()
            .unwrap();
        assert!(!r.cross_origin, "the default port is the same origin");
    }

    #[test]
    fn downgrades_and_foreign_schemes_are_refused() {
        let err = target(302, "http://api.example.com/x", Method::GET).unwrap_err();
        assert!(err.contains("cleartext"), "{err}");
        let err = target(302, "file:///etc/passwd", Method::GET).unwrap_err();
        assert!(err.contains("'file'"), "{err}");
        let plain = Url::parse("http://api.example.com/x").unwrap();
        assert!(
            redirect_target(&plain, 302, Some("http://other.example/x"), &Method::GET).is_ok(),
            "http to http is not a downgrade"
        );
    }
}
