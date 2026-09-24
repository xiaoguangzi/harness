//! HTTP plumbing shared by the protocol backends: POST with retries, and
//! an SSE reader.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::Value;

const MAX_RETRIES: u32 = 3;

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
            Err(e) => e.is_connect() || e.is_timeout(),
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
/// returns true (returns Ok(true)) or the stream ends (Ok(false)).
/// Non-JSON payloads such as `[DONE]` are skipped.
pub async fn read_sse(resp: reqwest::Response, mut on_data: impl FnMut(Value) -> Result<bool>) -> Result<bool> {
    // Buffer raw bytes: a chunk boundary can split a multi-byte UTF-8
    // character, so only decode once a whole event has arrived.
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response stream")?;
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
                if on_data(value)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
