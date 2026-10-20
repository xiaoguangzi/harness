//! HTTP plumbing shared by the protocol backends: POST, when to send a
//! failed request again, and an SSE reader.

use std::fmt;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flate2::{Compression, write::GzEncoder};
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// How many times a request that failed in passing is sent again.
pub const MAX_RETRIES: u32 = 3;
/// How many times, when the API is overloaded or rate-limited: those are
/// waited out for about five minutes.
pub const MAX_OVERLOAD_RETRIES: u32 = 10;
/// The longest wait between attempts without a `retry-after`.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A `retry-after` asking for longer than this is not waited for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// Bodies smaller than this go uncompressed.
const GZIP_MIN_BYTES: usize = 1024;

/// Marks an error after which the same request is worth sending again: the
/// connection failed or broke while the response streamed in, or the API
/// reported a passing failure. Its text is the error message (or the
/// context), so messages read as before.
#[derive(Debug)]
pub struct Transient {
    message: String,
    /// The API is overloaded or rate-limited, so it is waited out longer.
    overloaded: bool,
    /// How long the server asked to wait.
    retry_after: Option<Duration>,
}

impl Transient {
    pub fn new(message: String) -> Self {
        Self { message, overloaded: false, retry_after: None }
    }
}

impl fmt::Display for Transient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// When to send a request that failed with `error` again, as attempt
/// `attempt` (from 1), and how many attempts that kind of failure gets:
/// `None` if it is not [`Transient`] or its attempts are spent. The wait
/// doubles from 1 s up to a minute, unless the server said how long.
pub fn retry_delay(error: &anyhow::Error, attempt: u32) -> Option<(Duration, u32)> {
    let transient = error.downcast_ref::<Transient>()?;
    let max = if transient.overloaded { MAX_OVERLOAD_RETRIES } else { MAX_RETRIES };
    if attempt == 0 || attempt > max {
        return None;
    }
    let backoff = Duration::from_secs(1 << (attempt - 1).min(6)).min(MAX_BACKOFF);
    match transient.retry_after {
        Some(wait) if wait > MAX_RETRY_AFTER => None,
        Some(wait) => Some((wait, max)),
        None => Some((backoff, max)),
    }
}

/// An error the API reported in the stream; `kind` is its error type or
/// code. Overload and server errors are transient.
pub fn stream_error(message: String, kind: &str) -> anyhow::Error {
    match kind {
        "overloaded_error" | "rate_limit_error" | "rate_limit_exceeded" => {
            anyhow::Error::msg(Transient { message, overloaded: true, retry_after: None })
        }
        "api_error" | "server_error" => anyhow::Error::msg(Transient::new(message)),
        _ => anyhow::Error::msg(message),
    }
}

/// POSTs `body` as JSON, gzip-compressed with `gzip`. Only a successful
/// response is returned; a connection failure or a retryable status (408,
/// 409, 429, 5xx, 529) is a [`Transient`] error, for the caller to send the
/// request again.
pub async fn post(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    gzip: bool,
) -> Result<reqwest::Response> {
    let plain = serde_json::to_vec(body).context("serializing the request")?;
    let compressed = if gzip { compress(&plain)? } else { None };
    let mut req = http.post(url).header("content-type", "application/json");
    for (k, v) in headers {
        // These describe the bytes actually sent.
        if !k.eq_ignore_ascii_case("content-encoding") && !k.eq_ignore_ascii_case("content-length") {
            req = req.header(k, v);
        }
    }
    req = match compressed {
        Some(bytes) => req.header("content-encoding", "gzip").body(bytes),
        None => req.body(plain),
    };
    let resp = match req.send().await {
        Ok(resp) => resp,
        // Includes HTTP/2 streams reset before the response arrived.
        Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => {
            return Err(anyhow::Error::new(e).context(Transient::new(format!("sending request to {url}"))));
        }
        Err(e) => return Err(anyhow::Error::new(e).context(format!("sending request to {url}"))),
    };
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = retry_after(resp.headers());
    let text = resp.text().await.unwrap_or_default();
    let message = format!("API error {status} from {url}: {text}");
    Err(match status.as_u16() {
        429 | 529 => anyhow::Error::msg(Transient { message, overloaded: true, retry_after }),
        s @ (408 | 409 | 500..) => {
            let overloaded = s >= 500 && text.contains("overloaded");
            anyhow::Error::msg(Transient { message, overloaded, retry_after })
        }
        _ => anyhow::Error::msg(message),
    })
}

/// `body` gzip-compressed, unless it is small or compression saves nothing.
fn compress(body: &[u8]) -> Result<Option<Vec<u8>>> {
    if body.len() < GZIP_MIN_BYTES {
        return Ok(None);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(body).context("compressing the request")?;
    let compressed = encoder.finish().context("compressing the request")?;
    Ok((compressed.len() < body.len()).then_some(compressed))
}

/// The wait a response asks for: `retry-after-ms`, or `retry-after` in
/// seconds.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| {
        let value: f64 = headers.get(name)?.to_str().ok()?.trim().parse().ok()?;
        (value.is_finite() && value >= 0.0).then_some(value)
    };
    number("retry-after-ms").map(|ms| Duration::from_secs_f64(ms / 1000.0))
        .or_else(|| number("retry-after").map(Duration::from_secs_f64))
}

/// Feeds each SSE `data:` payload, parsed as JSON, to `on_data` until it
/// returns true. Non-JSON payloads such as `[DONE]` are skipped. A stream
/// that breaks off, or ends before `on_data` is done, is a [`Transient`]
/// error saying how far it got; `last` names the event that should end it.
pub async fn read_sse(resp: reqwest::Response, last: &str, mut on_data: impl FnMut(Value) -> Result<bool>) -> Result<()> {
    // Buffer raw bytes: a chunk boundary can split a multi-byte UTF-8
    // character, so only decode once a whole event has arrived.
    let mut buf: Vec<u8> = Vec::new();
    let mut progress = Progress::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| Transient::new(format!("reading response stream {progress}")))?;
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
    Err(anyhow::Error::msg(Transient::new(format!("stream ended before {last} {progress}"))))
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
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use serde_json::json;

    use super::*;

    fn transient(overloaded: bool, retry_after: Option<u64>) -> anyhow::Error {
        anyhow::Error::msg(Transient { message: "x".into(), overloaded, retry_after: retry_after.map(Duration::from_secs) })
    }

    #[test]
    fn overloads_are_waited_out_longer_than_other_failures() {
        let waits = |error: &anyhow::Error| -> Vec<u64> {
            (1..).map_while(|attempt| retry_delay(error, attempt)).map(|(wait, _)| wait.as_secs()).collect()
        };
        assert_eq!(waits(&transient(false, None)), [1, 2, 4]);
        assert_eq!(waits(&transient(true, None)), [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(waits(&transient(true, Some(7))).len(), 10, "the server's wait");
        assert_eq!(waits(&transient(true, Some(7)))[0], 7);
        assert!(retry_delay(&transient(true, Some(3600)), 1).is_none(), "too long to wait");
        assert!(retry_delay(&anyhow::anyhow!("API error 400"), 1).is_none());
        let wrapped = anyhow::Error::msg("inner").context(Transient::new("sending".into()));
        assert_eq!(retry_delay(&wrapped, 1), Some((Duration::from_secs(1), MAX_RETRIES)));
    }

    /// Answers each request with `status`, recording its encoding and body.
    async fn serve(status: StatusCode, headers: Vec<(&'static str, &'static str)>) -> (String, Arc<Mutex<Vec<(Option<String>, Vec<u8>)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |request_headers: HeaderMap, body: Bytes| async move {
                let encoding = request_headers.get("content-encoding").map(|v| v.to_str().unwrap().to_string());
                record.lock().unwrap().push((encoding, body.to_vec()));
                let mut response = axum::http::Response::builder().status(status);
                for (k, v) in headers {
                    response = response.header(k, v);
                }
                response.body(axum::body::Body::from("overloaded_error")).unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    #[tokio::test]
    async fn compresses_large_bodies_when_asked() {
        let (url, seen) = serve(StatusCode::OK, vec![]).await;
        let http = reqwest::Client::new();
        let big = json!({"text": "a".repeat(4000)});
        post(&http, &url, &[("Content-Encoding".into(), "br".into())], &big, true).await.unwrap();
        post(&http, &url, &[], &json!({"small": true}), true).await.unwrap();
        post(&http, &url, &[], &big, false).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0.as_deref(), Some("gzip"), "the configured header is replaced");
        let mut text = String::new();
        flate2::read::GzDecoder::new(&seen[0].1[..]).read_to_string(&mut text).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), big);
        assert_eq!(seen[1].0, None, "too small to compress");
        assert_eq!((seen[2].0.as_deref(), seen[2].1.len()), (None, big.to_string().len()));
    }

    #[tokio::test]
    async fn failed_statuses_say_whether_and_when_to_retry() {
        let http = reqwest::Client::new();
        let (url, _) = serve(StatusCode::from_u16(529).unwrap(), vec![("retry-after-ms", "1500")]).await;
        let error = post(&http, &url, &[], &json!({}), false).await.unwrap_err();
        assert!(error.to_string().starts_with("API error 529"), "{error}");
        assert_eq!(retry_delay(&error, 1), Some((Duration::from_millis(1500), MAX_OVERLOAD_RETRIES)));

        let (url, _) = serve(StatusCode::SERVICE_UNAVAILABLE, vec![]).await;
        let error = post(&http, &url, &[], &json!({}), false).await.unwrap_err();
        assert_eq!(retry_delay(&error, 1).unwrap().1, MAX_OVERLOAD_RETRIES, "the body says it is overloaded");

        let (url, _) = serve(StatusCode::BAD_REQUEST, vec![("retry-after", "1")]).await;
        let error = post(&http, &url, &[], &json!({}), false).await.unwrap_err();
        assert!(retry_delay(&error, 1).is_none());

        let error = post(&http, "http://127.0.0.1:9/", &[], &json!({}), false).await.unwrap_err();
        assert_eq!(retry_delay(&error, 1).unwrap().1, MAX_RETRIES, "{error:#}");
    }
}
