//! HTTP plumbing shared by the protocol backends: POST with retries, and
//! an SSE reader.

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::Value;

pub const MAX_RETRIES: u32 = 3;

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

/// POSTs `body` as JSON, retrying connection failures and retryable
/// statuses (408, 409, 429, 5xx). Only a successful response is returned.
pub async fn post_with_retries(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        let mut req = http.post(url).json(body);
        for (k, v) in headers {
            req = req.header(k, v);
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
            bail!("API error {status} from {url}: {text}");
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
pub async fn read_sse(resp: reqwest::Response, last: &str, mut on_data: impl FnMut(Value) -> Result<bool>) -> Result<()> {
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
