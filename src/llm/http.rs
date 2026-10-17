//! HTTP plumbing shared by the protocol backends: POST with retries, and
//! an SSE reader.

use std::collections::HashSet;
use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use flate2::{Compression, write::GzEncoder};
use futures::StreamExt;
use serde_json::Value;

pub const MAX_RETRIES: u32 = 3;
const GZIP_MIN_BYTES: usize = 1024;

/// Transport state shared by model switches and subagents. Cache by full
/// endpoint URL: a gateway may support Messages compression but not Responses.
#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    no_gzip: Arc<Mutex<HashSet<String>>>,
}

impl Client {
    pub fn new(inner: reqwest::Client) -> Self {
        Self { inner, no_gzip: Arc::default() }
    }

    /// A changed provider configuration may point at a newly upgraded gateway.
    pub fn reset_compression(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

fn gzip(body: &[u8]) -> Result<Option<Vec<u8>>> {
    if body.len() < GZIP_MIN_BYTES {
        return Ok(None);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(body).context("compressing API request")?;
    let compressed = encoder.finish().context("finishing API request compression")?;
    Ok((compressed.len() < body.len()).then_some(compressed))
}

/// Some relays ignore Content-Encoding and try to parse gzip bytes as JSON.
/// A successful plain retry is required before remembering any rejection.
fn compression_rejected(status: u16, text: &str) -> bool {
    if status == 415 {
        return true;
    }
    if !matches!(status, 400 | 422) {
        return false;
    }
    let text = text.to_ascii_lowercase();
    [
        "gzip",
        "content-encoding",
        "content encoding",
        "decompress",
        "incorrect header check",
        "invalid json",
        "json decode",
        "json_decode",
        "json parse",
        "json_parse",
        "parse json",
        "parsing json",
        "decode json",
        "json_invalid",
        "jsondecodeerror",
        "failed to parse the request body as json",
        "invalid character",
        "invalid start byte",
        "utf-8",
        "utf8",
    ]
    .iter()
    .any(|hint| text.contains(hint))
}

/// Marks an error after which the same request is worth sending again: the
/// connection broke while the response streamed in, or the API reported a
/// passing failure mid-stream. Its text is the error message (or the
/// context), so messages read as before.
#[derive(Debug)]
pub struct Transient(pub String);

impl fmt::Display for Transient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `error` is marked [`Transient`].
pub fn is_transient(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Transient>().is_some()
}

/// An error the API reported in the stream; `kind` is its error type or
/// code. Overload and server errors are transient.
pub fn stream_error(message: String, kind: &str) -> anyhow::Error {
    match kind {
        "overloaded_error" | "api_error" | "server_error" | "rate_limit_error" | "rate_limit_exceeded" => {
            anyhow::Error::msg(Transient(message))
        }
        _ => anyhow::Error::msg(message),
    }
}

/// POSTs JSON, trying gzip for larger bodies. An encoding/JSON rejection
/// falls back to the original bytes once; success disables gzip for this
/// endpoint until configuration reload or restart. Connection failures and
/// retryable statuses (408, 409, 429, 5xx) keep the existing retry policy.
pub async fn post_with_retries(
    http: &Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<reqwest::Response> {
    let plain = serde_json::to_vec(body).context("serializing API request")?;
    let compressed = if http.no_gzip.lock().unwrap().contains(url) { None } else { gzip(&plain)? };
    let mut use_gzip = compressed.is_some();
    let mut fell_back = false;
    let mut attempt = 0;
    loop {
        // Another concurrent request may have discovered an incompatible relay.
        if use_gzip && http.no_gzip.lock().unwrap().contains(url) {
            use_gzip = false;
        }
        let bytes = if use_gzip { compressed.as_ref().unwrap() } else { &plain };
        let mut req = http.inner.post(url).header("content-type", "application/json").body(bytes.clone());
        for (k, v) in headers {
            // These headers describe the actual wire bytes, including on fallback.
            if !k.eq_ignore_ascii_case("content-encoding") && !k.eq_ignore_ascii_case("content-length") {
                req = req.header(k, v);
            }
        }
        if use_gzip {
            req = req.header("content-encoding", "gzip");
        }
        let result = req.send().await;
        let retryable = match &result {
            Ok(resp) => {
                let s = resp.status().as_u16();
                s == 408 || s == 409 || s == 429 || s >= 500
            }
            // Includes HTTP/2 streams reset before the response arrived.
            Err(e) => e.is_connect() || e.is_timeout() || e.is_request(),
        };
        if retryable && attempt < MAX_RETRIES {
            attempt += 1;
            let wait = retry_after(&result).unwrap_or(Duration::from_secs(1 << attempt));
            tokio::time::sleep(wait).await;
            continue;
        }

        let resp = result.with_context(|| format!("sending request to {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            if use_gzip && compression_rejected(status.as_u16(), &text) {
                use_gzip = false;
                fell_back = true;
                continue;
            }
            bail!("API error {status} from {url}: {text}");
        }
        if fell_back {
            http.no_gzip.lock().unwrap().insert(url.to_string());
        }
        return Ok(resp);
    }
}

fn retry_after(result: &reqwest::Result<reqwest::Response>) -> Option<Duration> {
    let secs = result.as_ref().ok()?.headers().get("retry-after")?.to_str().ok()?.parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Feeds each SSE `data:` payload, parsed as JSON, to `on_data` until it
/// returns true. Non-JSON payloads such as `[DONE]` are skipped. A stream
/// that breaks off, or ends before `on_data` is done, is a [`Transient`]
/// error saying how far it got; `last` names the event that should end it.
pub async fn read_sse(
    resp: reqwest::Response,
    last: &str,
    mut on_data: impl FnMut(Value) -> Result<bool>,
) -> Result<()> {
    // Buffer raw bytes: a chunk boundary can split a multi-byte UTF-8
    // character, so only decode once a whole event has arrived.
    let mut buf: Vec<u8> = Vec::new();
    let mut progress = Progress::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| Transient(format!("reading response stream {progress}")))?;
        progress.last_data = Instant::now();
        buf.extend(chunk.iter().filter(|&&b| b != b'\r'));
        // An SSE event ends with a blank line.
        while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
            let raw: Vec<u8> = buf.drain(..end + 2).collect();
            let event = String::from_utf8_lossy(&raw);
            for data in event.lines().filter_map(|l| l.strip_prefix("data:")) {
                let data = data.trim();
                if !data.starts_with('{') {
                    continue;
                }
                let value: Value =
                    serde_json::from_str(data).with_context(|| format!("parsing stream event: {data}"))?;
                progress.saw(&value);
                if on_data(value)? {
                    return Ok(());
                }
            }
        }
    }
    Err(anyhow::Error::msg(Transient(format!("stream ended before {last} {progress}"))))
}

/// How far a response stream got, for errors: whether it broke off after a
/// long silence (an idle timeout on the way) or while data was flowing.
struct Progress {
    start: Instant,
    last_data: Instant,
    events: usize,
    /// The latest event's type, with its delta's type if it has one.
    last_event: String,
}

impl Progress {
    fn new() -> Self {
        let now = Instant::now();
        Self { start: now, last_data: now, events: 0, last_event: String::new() }
    }

    fn saw(&mut self, event: &Value) {
        self.events += 1;
        self.last_event.clear();
        self.last_event.push_str(event["type"].as_str().unwrap_or("?"));
        if let Some(delta) = event["delta"]["type"].as_str() {
            self.last_event.push('/');
            self.last_event.push_str(delta);
        }
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (total, idle) = (self.start.elapsed().as_secs_f64(), self.last_data.elapsed().as_secs_f64());
        write!(f, "after {total:.1}s, {idle:.1}s since the last data ({} events", self.events)?;
        if self.events > 0 {
            write!(f, ", the last {}", self.last_event)?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Read;

    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use serde_json::json;

    use super::*;

    struct Request {
        path: String,
        headers: HeaderMap,
        body: Bytes,
    }

    impl Request {
        fn compressed(&self) -> bool {
            self.headers.get("content-encoding").is_some_and(|v| v == "gzip")
        }

        fn json(&self) -> Value {
            let mut bytes = Vec::new();
            if self.compressed() {
                flate2::read::GzDecoder::new(self.body.as_ref()).read_to_end(&mut bytes).unwrap();
            } else {
                bytes.extend_from_slice(&self.body);
            }
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    struct Server {
        url: String,
        requests: Arc<Mutex<Vec<Request>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve(answers: Vec<(u16, &'static str)>) -> Server {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let answers = Arc::new(Mutex::new(VecDeque::from(answers)));
        let app =
            axum::Router::new().fallback(axum::routing::post(move |uri: Uri, headers: HeaderMap, body: Bytes| {
                captured.lock().unwrap().push(Request { path: uri.path().into(), headers, body });
                let (status, text) = answers.lock().unwrap().pop_front().expect("unexpected extra request");
                async move {
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("retry-after", "0"), ("content-type", "text/event-stream")],
                        text,
                    )
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { url, requests, task }
    }

    fn client() -> Client {
        Client::new(reqwest::Client::builder().no_proxy().build().unwrap())
    }

    fn body() -> Value {
        json!({"model": "test", "stream": true, "messages": [{"role": "user", "content": "压缩上下文 🦀 ".repeat(1000)}]})
    }

    #[tokio::test]
    async fn gzip_round_trip_preserves_json_headers_and_sse() {
        let server = serve(vec![(200, "data: {\"type\":\"done\"}\n\n"), (200, "ok")]).await;
        let client = client();
        let body = body();
        let headers = vec![
            ("authorization".into(), "Bearer fixture".into()),
            ("x-team".into(), "test".into()),
            ("Content-Length".into(), "999999".into()),
            ("Content-Encoding".into(), "br".into()),
        ];
        let response = post_with_retries(&client, &server.url, &headers, &body).await.unwrap();
        read_sse(response, "done", |event| Ok(event["type"] == "done")).await.unwrap();
        post_with_retries(&client, &server.url, &headers, &body).await.unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(request.compressed());
            assert_eq!(request.json(), body);
            assert!(request.body.len() < serde_json::to_vec(&body).unwrap().len());
            assert_eq!(request.headers["content-type"], "application/json");
            assert_eq!(request.headers["authorization"], "Bearer fixture");
            assert_eq!(request.headers["x-team"], "test");
            assert_eq!(
                request.headers["content-length"].to_str().unwrap().parse::<usize>().unwrap(),
                request.body.len()
            );
            assert_eq!(request.headers.get_all("content-encoding").iter().count(), 1);
        }
    }

    #[tokio::test]
    async fn rejects_gzip_then_remembers_plain_success_for_clones() {
        for (status, error) in [
            (415, "unsupported media type"),
            (400, "invalid character '\\x1f' looking for beginning of value"),
            (422, "{\"type\":\"json_invalid\",\"msg\":\"JSON decode error\"}"),
            (400, "unsupported content encoding"),
        ] {
            let server = serve(vec![(status, error), (200, "ok"), (200, "ok")]).await;
            let client = client();
            let body = body();
            post_with_retries(&client, &server.url, &[], &body).await.unwrap();
            post_with_retries(&client.clone(), &server.url, &[], &body).await.unwrap();
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.iter().map(Request::compressed).collect::<Vec<_>>(), [true, false, false]);
            for request in requests.iter() {
                assert_eq!(request.json(), body);
                if !request.compressed() {
                    assert!(!request.headers.contains_key("content-encoding"));
                    assert_eq!(request.body.as_ref(), serde_json::to_vec(&body).unwrap());
                }
            }
        }
    }

    #[tokio::test]
    async fn unrelated_errors_do_not_fallback_or_disable_gzip() {
        for (status, error) in
            [(400, "unknown model"), (422, "invalid tool schema"), (401, "gzip not authorized"), (403, "forbidden")]
        {
            let server = serve(vec![(status, error), (200, "ok")]).await;
            let client = client();
            let error = post_with_retries(&client, &server.url, &[], &body()).await.unwrap_err();
            assert!(error.to_string().contains(&status.to_string()));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
            post_with_retries(&client, &server.url, &[], &body()).await.unwrap();
            assert!(server.requests.lock().unwrap().iter().all(Request::compressed));
        }
    }

    #[tokio::test]
    async fn failed_plain_retry_is_not_cached_and_does_not_loop() {
        let server = serve(vec![(400, "invalid JSON schema"), (400, "invalid JSON schema"), (200, "ok")]).await;
        let client = client();
        assert!(post_with_retries(&client, &server.url, &[], &body()).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        post_with_retries(&client, &server.url, &[], &body()).await.unwrap();
        assert_eq!(
            server.requests.lock().unwrap().iter().map(Request::compressed).collect::<Vec<_>>(),
            [true, false, true]
        );
    }

    #[tokio::test]
    async fn transient_errors_keep_gzip_and_existing_retry_budget() {
        for status in [408, 409, 429, 500, 503] {
            let server = serve(vec![(status, "gzip unavailable"), (200, "ok")]).await;
            post_with_retries(&client(), &server.url, &[], &body()).await.unwrap();
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            assert!(server.requests.lock().unwrap().iter().all(Request::compressed));
        }
        let server = serve(vec![(503, "overloaded"); MAX_RETRIES as usize + 1]).await;
        assert!(post_with_retries(&client(), &server.url, &[], &body()).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), MAX_RETRIES as usize + 1);
    }

    #[tokio::test]
    async fn fallback_does_not_spend_transient_retry_budget() {
        let mut answers = vec![(429, "slow down"); MAX_RETRIES as usize];
        answers.extend([(415, "unsupported"), (200, "ok")]);
        let server = serve(answers).await;
        post_with_retries(&client(), &server.url, &[], &body()).await.unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), MAX_RETRIES as usize + 2);
        assert!(requests[..requests.len() - 1].iter().all(Request::compressed));
        assert!(!requests.last().unwrap().compressed());
    }

    #[tokio::test]
    async fn cache_is_scoped_to_endpoint_and_reset_on_config_reload() {
        let server = serve(vec![(415, "unsupported"), (200, "ok"), (200, "ok"), (200, "ok"), (200, "ok")]).await;
        let client = client();
        let messages = format!("{}/v1/messages", server.url);
        let responses = format!("{}/v1/responses", server.url);
        post_with_retries(&client, &messages, &[], &body()).await.unwrap();
        post_with_retries(&client, &responses, &[], &body()).await.unwrap();
        post_with_retries(&client, &messages, &[], &body()).await.unwrap();
        post_with_retries(&client.reset_compression(), &messages, &[], &body()).await.unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.iter().map(Request::compressed).collect::<Vec<_>>(), [true, false, true, false, true]);
        assert_eq!(requests[2].path, "/v1/responses");
    }

    #[tokio::test]
    async fn small_requests_stay_plain_without_disabling_later_compression() {
        let server = serve(vec![(200, "ok"), (200, "ok")]).await;
        let client = client();
        post_with_retries(&client, &server.url, &[], &json!({"input": "hi"})).await.unwrap();
        post_with_retries(&client, &server.url, &[], &body()).await.unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.iter().map(Request::compressed).collect::<Vec<_>>(), [false, true]);
    }

    #[test]
    fn compression_skips_small_or_incompressible_bytes() {
        assert!(gzip(&vec![b'a'; GZIP_MIN_BYTES - 1]).unwrap().is_none());
        assert!(gzip(&vec![b'a'; GZIP_MIN_BYTES]).unwrap().is_some());
        let mut state = 42u32;
        let bytes: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        assert!(gzip(&bytes).unwrap().is_none());
    }

    #[tokio::test]
    async fn both_protocols_use_gzip_and_fallback_through_the_model_client() {
        use crate::llm::config::tests::{file, no_env};
        use crate::llm::{Message, Providers};

        for (protocol, path, events) in [
            (
                "anthropic",
                "/v1/messages",
                concat!(
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"ok\"}}\n\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                ),
            ),
            (
                "openai-responses",
                "/v1/responses",
                concat!(
                    "data: {\"type\":\"response.completed\",\"response\":{\"output\":[",
                    "{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n"
                ),
            ),
        ] {
            let server = serve(vec![(415, "unsupported encoding"), (200, events), (200, events)]).await;
            let providers = Providers::from_parts(
                &file(json!({
                    "model": "mock/m", "providers": {"mock": {
                        "protocol": protocol, "base_url": server.url, "api_key": "fixture-only"
                    }}
                })),
                &no_env,
            )
            .unwrap();
            let client = crate::llm::Client::new(providers).unwrap();
            let messages = vec![Message::user(vec![json!({"type": "text", "text": "你好 ".repeat(1000)})])];
            for client in [client.clone(), client.switch("mock/another").unwrap()] {
                let response = client.create("system", &[], &messages, &mut |_| {}).await.unwrap();
                assert_eq!(response.content, vec![json!({"type": "text", "text": "ok"})]);
            }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.iter().map(Request::compressed).collect::<Vec<_>>(), [true, false, false]);
            assert!(requests.iter().all(|request| request.path == path));
            assert_eq!(requests[0].json(), requests[1].json());
            assert_eq!(requests[2].json()["model"], "another");
        }
    }
}
