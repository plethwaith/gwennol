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
/// holding it open.
///
/// The idle limit is per chunk: it catches a peer that stops talking, which
/// is the failure a long-lived SSE stream actually has. A total budget would
/// be wrong here — a model streaming a long answer is working, not stuck.
///
/// Cancelling the invocation is deliberately not this function's job.
/// gwead's own registry-side read release (`STREAM_CANCELLED`) already
/// tears a parked read down, using the very token a consumer's
/// `stream_read` carries; racing that same token in here too would only
/// pre-empt it — the wrapped source would resolve *ready* with an error
/// item the instant the token fired, and gwead polls the source before
/// the token, so the release would never be the one seen.
///
/// Empty chunks are dropped here rather than handed to the consumer, so
/// gwead's own read loop never sees one: gwead skips an empty chunk
/// *inside* one `read_async` call instead of returning to ask again, so
/// a source that is always immediately ready with them never lets a
/// fired token win the race (tracked upstream as gwead#22). This is
/// defense-in-depth against a source contract this repo does not
/// itself enforce — nothing pins a raw `reqwest::bytes_stream()`'s
/// chunks as non-empty the way a `gwennol-guest` guest's
/// `Stream::write_all` pins its own writes — rather than a fix for a
/// threat live against this build: `h2` is absent from this
/// workspace's lockfile and `reqwest` is built without the `http2`
/// feature, and over HTTP/1.1 chunked encoding a size-0 chunk *is* the
/// terminator, so a non-terminal empty chunk from a real peer, hostile
/// or not, cannot reach here today. The guard exists so a future flip
/// of that feature does not silently make the residual live.
///
/// `yield_now` after a dropped chunk is load-bearing, not a courtesy to
/// other tasks: it is what makes this call return `Pending` at least
/// once. `source.next()` is what gwead's own `read_async` races the
/// cancellation token against, `biased` toward source — a source that
/// is always immediately ready with empty chunks would resolve *this*
/// call `Ready` on every poll if the empty-chunk skip above ran with
/// no suspension point of its own, so the race would never reach the
/// token at all. (That is the counterfactual for removing `yield_now`
/// specifically; removing the whole skip arm instead hands such a
/// chunk to the consumer, a different failure entirely.) The first
/// poll of `yield_now` returns `Pending`, which propagates out through
/// this whole `.await` chain as this call's own result for that poll,
/// letting gwead's select fall through to the token this time. This is
/// not what gwead's own identically-named call in its skip loop does:
/// that `yield_now` races the *same* select every iteration and can
/// never let its token win one — the source there is polled inside the
/// very race being raced, biased toward itself — so it only keeps
/// *other* tasks scheduled while gwead#22 stays open. Here, this call
/// entirely *is* the source gwead's select polls, so making it
/// `Pending` is the whole fix, not a side effect of one.
///
/// The per-chunk idle deadline is computed once per real chunk sought,
/// not once per host call: an empty chunk restarting it would let a
/// *trickling* peer that never stops sending them evade
/// `idle_timeout_ms` forever, so the timer counts the wait for a chunk
/// with something in it, empties included in that wait. This bounds
/// only a peer whose gaps between chunks are real enough for
/// `source.next()` to suspend; the always-ready case above never
/// reaches a deadline check at all — `tokio::time::timeout_at` only
/// consults its clock once the future it wraps returns `Pending`, and
/// an always-ready source never does.
fn guarded_body(source: ReadableSource, idle: Duration) -> ReadableSource {
    Box::pin(gwead::futures::stream::unfold(
        Some(source),
        move |state| async move {
            let mut source = state?;
            let deadline = tokio::time::Instant::now() + idle;
            loop {
                match tokio::time::timeout_at(deadline, source.next()).await {
                    Ok(Some(Ok(bytes))) if bytes.is_empty() => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                    Ok(Some(item)) => {
                        let keep = item.is_ok().then_some(source);
                        return Some((item, keep));
                    }
                    Ok(None) => return None,
                    Err(_) => {
                        return Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                format!("no response data for {idle:?}"),
                            )),
                            None,
                        ));
                    }
                }
            }
        },
    ))
}

/// Strip everything from a URL that can carry a credential — userinfo,
/// query, fragment — leaving scheme, host and path. Every URL headed for
/// a plugin-visible string or a log goes through here, and a frontend
/// writing its own log of an [`Access::Http`](crate::Access) should
/// use it too: the operator judges the full URL, but a trace is a
/// record, and a key in a query string does not belong in one.
pub fn scrub(u: &mut Url) {
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
/// the body as well when it is buffered. A streamed body is bounded
/// instead by `idle_timeout_ms` between chunks; the invocation's
/// cancellation token bounds a *consumer's* read of it, releasing one
/// parked on a source that has gone quiet as `STREAM_CANCELLED`. The
/// connection itself ends when the consumer closes or drops its
/// handle, or — when the kernel still owns the stream table at the end
/// of the driving action — the kernel's own post-invocation drain
/// force-closes it. Two dispatch paths hand a consumer a streamed
/// handle and disagree on which: `with_streams` (the agent loop's own
/// streamed reads) supplies the caller's own table, which disables
/// that drain for the whole call, so a consumer must close what it
/// opens; `into_dataflow_streaming_handle` allocates its own table and
/// leaves the drain enabled, so it is what ends a handle nothing else
/// closed.
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
                .register_readable(content_type, guarded_body(source, idle));
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

    /// A vendor body that never sends anything but empty chunks must
    /// not starve a fired cancellation token the way an always-ready
    /// source can (gwead#22): `guarded_body` drops empty chunks itself
    /// so gwead's own read loop, which skips them *inside* one call
    /// rather than returning to ask again, never sees one to skip.
    ///
    /// Bounded and sentinel-based, not timeout-based: plan section 6
    /// forbids relying on a timeout to catch a spin, and half of this
    /// fix's own regression — `yield_now` deleted but the `continue`
    /// kept — spins synchronously inside `guarded_body`, in the same
    /// task as a `tokio::time::timeout` wrapped around the read, so
    /// such a timeout is never even polled and the suite wedges
    /// instead of failing red. A finite flood followed by a real
    /// sentinel chunk fails on a wrong *value* the instant the fix (or
    /// half of it) is reverted, whether the revert hangs or not.
    #[tokio::test]
    async fn an_always_empty_body_does_not_starve_a_fired_token() {
        use gwead::bytes::Bytes;
        use gwead::kernel::streams::{STREAM_CANCELLED, StreamRegistry, read_async_shared};
        use gwead::tokio_util::sync::CancellationToken;

        let mut items: Vec<std::io::Result<Bytes>> =
            (0..10_000).map(|_| Ok(Bytes::new())).collect();
        items.push(Ok(Bytes::from_static(b"payload")));
        let source: ReadableSource = Box::pin(gwead::futures::stream::iter(items));
        let guarded = guarded_body(source, Duration::from_secs(60));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/octet-stream", guarded);
        let streams = std::sync::Arc::new(std::sync::Mutex::new(registry));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut buf = [0u8; 8];
        let n = read_async_shared(&streams, id, &mut buf, &cancel).await;
        assert_eq!(n, STREAM_CANCELLED, "got n={n}");
    }

    /// Dropping empty chunks must not drop or reorder the real ones
    /// around them: a mutated `bytes.is_empty()` guard that also
    /// matched some real chunks would truncate a body with the
    /// all-empty starvation test above still green. This does not
    /// independently prove *this function's* skip is what runs —
    /// gwead's own `read_async` skips empty chunks too, so the same
    /// bytes would survive with this arm disabled entirely — only that
    /// nothing here loses or reorders data around one.
    #[tokio::test]
    async fn empty_chunks_interleaved_with_data_are_dropped_without_losing_bytes() {
        use gwead::bytes::Bytes;
        use gwead::kernel::streams::{STREAM_EOF, StreamRegistry, read_async_shared};
        use gwead::tokio_util::sync::CancellationToken;

        let items: Vec<std::io::Result<Bytes>> = vec![
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"hi")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"there")),
            Ok(Bytes::new()),
        ];
        let source: ReadableSource = Box::pin(gwead::futures::stream::iter(items));
        let guarded = guarded_body(source, Duration::from_secs(60));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/octet-stream", guarded);
        let streams = std::sync::Arc::new(std::sync::Mutex::new(registry));
        let cancel = CancellationToken::new();
        let mut buf = [0u8; 16];
        assert_eq!(read_async_shared(&streams, id, &mut buf, &cancel).await, 2);
        assert_eq!(&buf[..2], b"hi");
        assert_eq!(read_async_shared(&streams, id, &mut buf, &cancel).await, 5);
        assert_eq!(&buf[..5], b"there");
        assert_eq!(
            read_async_shared(&streams, id, &mut buf, &cancel).await,
            STREAM_EOF
        );
    }

    /// An error item past some empty chunks still surfaces: the skip
    /// arm must not swallow anything but a genuinely empty `Ok`.
    #[tokio::test]
    async fn an_error_item_past_empty_chunks_still_surfaces() {
        use gwead::bytes::Bytes;
        use gwead::kernel::streams::{STREAM_IO_ERROR, StreamRegistry, read_async_shared};
        use gwead::tokio_util::sync::CancellationToken;

        let items: Vec<std::io::Result<Bytes>> = vec![
            Ok(Bytes::new()),
            Err(std::io::Error::other("boom")),
            Ok(Bytes::from_static(b"after")),
        ];
        let source: ReadableSource = Box::pin(gwead::futures::stream::iter(items));
        let guarded = guarded_body(source, Duration::from_secs(60));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/octet-stream", guarded);
        let streams = std::sync::Arc::new(std::sync::Mutex::new(registry));
        let cancel = CancellationToken::new();
        let mut buf = [0u8; 16];
        assert_eq!(
            read_async_shared(&streams, id, &mut buf, &cancel).await,
            STREAM_IO_ERROR
        );
    }

    /// The idle deadline is computed once per real chunk sought, not
    /// restarted by every skipped empty one: a *trickling* peer that
    /// sends only empty chunks, spaced out enough for `source.next()`
    /// to genuinely suspend between them, must not be able to evade
    /// `idle_timeout_ms` just by never sending anything real. (An
    /// always-ready peer is a different case entirely — see
    /// `an_always_empty_body_does_not_starve_a_fired_token`'s doc — and
    /// this test does not claim to bound one.)
    ///
    /// `start_paused = true` runs this against a mocked clock: the
    /// 5ms-per-chunk margin against the 50ms deadline is exact and
    /// immune to real scheduling jitter, unlike a real-time sleep
    /// under load, which can stretch enough to make the fixed and
    /// regressed code trip identically. Bounded by a fixed item count
    /// too, not an outer timeout: an unbounded trickle would still run
    /// forever against a reverted hoist, mocked clock or not, since
    /// nothing would ever reach a value to assert on. 40 empty chunks
    /// at 5ms is 200ms of (virtual) delay if all are consumed —
    /// comfortably past the 50ms deadline — so a reverted hoist reaches
    /// the real chunk after item 40 and this fails on a **value**, with
    /// no timeout anywhere and no dependence on wall-clock time at all.
    #[tokio::test(start_paused = true)]
    async fn a_trickling_empty_chunk_flood_still_trips_the_idle_timeout() {
        use gwead::bytes::Bytes;
        use gwead::kernel::streams::{STREAM_IO_ERROR, StreamRegistry, read_async_shared};
        use gwead::tokio_util::sync::CancellationToken;

        let source: ReadableSource =
            Box::pin(gwead::futures::stream::unfold(0usize, |i| async move {
                if i < 40 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Some((Ok(Bytes::new()), i + 1))
                } else {
                    Some((Ok(Bytes::from_static(b"payload")), i + 1))
                }
            }));
        let guarded = guarded_body(source, Duration::from_millis(50));
        let mut registry = StreamRegistry::new();
        let id = registry.register_readable("application/octet-stream", guarded);
        let streams = std::sync::Arc::new(std::sync::Mutex::new(registry));
        let cancel = CancellationToken::new();
        let mut buf = [0u8; 8];
        let n = read_async_shared(&streams, id, &mut buf, &cancel).await;
        assert_eq!(n, STREAM_IO_ERROR, "got n={n}");
    }
}
