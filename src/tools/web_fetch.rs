//! `web_fetch`: a web page as Markdown, for documentation, an issue or what
//! an error message means. Claude Code's WebFetch has a small model answer
//! a prompt about the page; this returns the page itself, or its start and
//! end with the whole in a file to `read` or `grep`, as other large outputs.

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use encoding_rs::Encoding;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Tool, ToolOutput, parse_input, scheduling, spill};

pub const NAME: &str = "web_fetch";
/// Past this the page is cut off rather than read on.
const MAX_BODY_BYTES: usize = 10 << 20;
/// What the model gets of a longer page: its start and end.
const MAX_OUTPUT_BYTES: usize = 40_000;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Redirects within the host followed, at most.
const MAX_REDIRECTS: usize = 5;
/// Elements with nothing to read in them, or only a site's chrome.
const SKIPPED: [&str; 12] =
    ["script", "style", "noscript", "svg", "template", "iframe", "head", "canvas", "img", "picture", "nav", "footer"];

pub struct WebFetchTool {
    /// Whether loopback and private addresses may be fetched: only tests'
    /// local servers.
    local: bool,
    client: OnceLock<Result<reqwest::Client, String>>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self { local: false, client: OnceLock::new() }
    }

    fn client(&self) -> Result<&reqwest::Client, String> {
        let client = self.client.get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(TIMEOUT)
                .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION"), " (web_fetch)"))
                .build()
                .map_err(|e| format!("{e:#}"))
        });
        client.as_ref().map_err(Clone::clone)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    url: String,
}

/// Why `url` is not fetched, if it is not: only public web addresses, as
/// Claude Code's WebFetch, not a service on this machine or the local
/// network (the web UI's API, a router's).
fn refused(url: &Url, local: bool) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Some(format!("Only http and https URLs can be fetched, not {}:.", url.scheme()));
    }
    let host = url.host_str()?;
    if local {
        return None;
    }
    // `localhost.` is `localhost`.
    let name = host.trim_end_matches('.');
    let private = match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(ip) => is_local(ip),
        Err(_) => !name.contains('.') || name.ends_with(".localhost") || name.ends_with(".local"),
    };
    private.then(|| format!("{host} is on this machine or the local network; web_fetch fetches public web pages only."))
}

/// Whether `ip` is this machine's or the local network's.
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast()
                || a == 0 || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

/// Why `url`'s host is not fetched once it is looked up: a public name may
/// point at this machine or the local network. Checked before each request
/// (a redirect's too); the connection looks the name up again, so a name
/// that changes its answer in between is not caught.
async fn resolves_locally(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    if host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        return None;
    }
    let port = url.port_or_known_default().unwrap_or(443);
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.map(|a| a.ip()).find(|ip| is_local(*ip)).map(|ip| {
            format!("{host} points at {ip}, on this machine or the local network; web_fetch fetches public web pages only.")
        }),
        Err(e) => Some(format!("{host} could not be looked up: {e}")),
    }
}

/// Whether a redirect from `from` to `to` stays on the site: the same host,
/// give or take `www.`.
fn same_site(from: &Url, to: &Url) -> bool {
    let bare = |u: &Url| u.host_str().map(|h| h.strip_prefix("www.").unwrap_or(h).to_string());
    bare(from) == bare(to)
}

/// `body` as text, in the charset named by a byte-order mark, else by
/// `content_type`, else, in HTML or XML, by a `<meta>` or XML declaration
/// near the page's start; UTF-8 if none is, or the name is unknown.
fn decode<'a>(content_type: &str, body: &'a [u8]) -> Cow<'a, str> {
    static DECLARED: OnceLock<regex::Regex> = OnceLock::new();
    let declared = DECLARED.get_or_init(|| {
        let value = r#"\s*=\s*["']?\s*([a-z0-9._:-]+)"#;
        regex::Regex::new(&format!(r"(?i)<meta\s[^>]*?\bcharset{value}|<\?xml\s[^>]*?\bencoding{value}")).unwrap()
    });
    let mut params = content_type.split(';');
    let kind = params.next().unwrap_or_default().trim().to_ascii_lowercase();
    let header = params.find_map(|p| {
        let (name, value) = p.split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['"', '\'']).to_string())
    });
    let encoding = match header.and_then(|l| Encoding::for_label(l.as_bytes())) {
        Some(encoding) => encoding,
        None if kind.is_empty() || kind.contains("html") || kind.contains("xml") => {
            let start = String::from_utf8_lossy(&body[..body.len().min(4096)]);
            let label = declared.captures(&start).and_then(|c| c.get(1).or_else(|| c.get(2))).map(|m| m.as_str().to_string());
            // A page that says it is UTF-16 in ASCII is not (as browsers
            // take it).
            label.and_then(|l| Encoding::for_label(l.as_bytes())).map_or(encoding_rs::UTF_8, Encoding::output_encoding)
        }
        None => encoding_rs::UTF_8,
    };
    // A byte-order mark goes before both.
    encoding.decode(body).0
}

/// The page's text: Markdown for HTML, as it is for other text; `Err` for
/// what is not text.
fn page_text(content_type: &str, body: &[u8]) -> Result<String, String> {
    let kind = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let text = decode(content_type, body);
    let k = kind.as_str();
    let html = matches!(k, "text/html" | "application/xhtml+xml") || (k.is_empty() && text.trim_start().starts_with('<'));
    if html {
        let converter = htmd::HtmlToMarkdown::builder().skip_tags(SKIPPED.to_vec()).build();
        let body = converter.convert(&text).map_err(|e| format!("The page's HTML could not be read: {e}"))?;
        let title = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok().and_then(|re| re.captures(&text).map(|c| c[1].to_string()));
        let title = title.and_then(|t| htmd::convert(&t).ok()).map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
        return Ok(match title.filter(|t| !t.is_empty()) {
            Some(title) => format!("Title: {title}\n\n{}", body.trim()),
            None => body,
        });
    }
    let textual = k.is_empty() || k.starts_with("text/") || ["json", "xml", "javascript"].iter().any(|t| k.ends_with(t));
    if textual { Ok(text.into_owned()) } else { Err(format!("The URL returned {k}, which web_fetch cannot show as text.")) }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Fetches a public web page and returns it as Markdown (other text, such as JSON, as it is). An http URL \
         is fetched over https. A redirect to another host is not followed: you are told where it goes, to fetch \
         that if you want it. A long page comes back as its start and end, with the whole in a file to read or \
         grep. Not for pages behind a login, nor for localhost or the local network."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "The page's full URL."}},
            "required": ["url"],
            "additionalProperties": false
        })
    }

    fn access(&self, _input: &Value) -> scheduling::Access {
        scheduling::Access::Concurrent
    }

    /// Claude Code's WebFetch also takes the `prompt` its small model
    /// answers; the whole page comes back here.
    fn normalize_input(&self, input: &mut Map<String, Value>) {
        input.remove("prompt");
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(e) => return e,
        };
        if crate::security::sandbox::active().is_some_and(|s| !s.allows_network()) {
            return ToolOutput::error("The sandbox allows no network access, so web_fetch cannot fetch pages.");
        }
        let mut url = match Url::parse(input.url.trim()) {
            Ok(url) => url,
            Err(e) => return ToolOutput::error(format!("Not a URL: {e}")),
        };
        let client = match self.client() {
            Ok(client) => client,
            Err(e) => return ToolOutput::error(format!("Could not set up the HTTP client: {e}")),
        };
        let mut redirects = 0;
        let response = loop {
            // A redirect's too.
            if url.scheme() == "http" && !self.local {
                let _ = url.set_scheme("https");
            }
            if let Some(reason) = refused(&url, self.local) {
                return ToolOutput::error(reason);
            }
            if !self.local
                && let Some(reason) = resolves_locally(&url).await
            {
                return ToolOutput::error(reason);
            }
            let request = client.get(url.clone()).header("accept", "text/html, text/markdown, text/plain, */*;q=0.5");
            let response = match request.send().await {
                Ok(response) => response,
                Err(e) => return ToolOutput::error(format!("Fetching {url} failed: {e:#}")),
            };
            if !response.status().is_redirection() {
                break response;
            }
            let location = response.headers().get("location").and_then(|l| l.to_str().ok()).and_then(|l| url.join(l).ok());
            let Some(next) = location else {
                return ToolOutput::error(format!("{url} answered {} without saying where to.", response.status()));
            };
            if !same_site(&url, &next) {
                return ToolOutput::ok(format!(
                    "{url} redirects to {next}, on another host. Call web_fetch with that URL to fetch it."
                ));
            }
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return ToolOutput::error(format!("{url} redirected more than {MAX_REDIRECTS} times."));
            }
            url = next;
        };
        let status = response.status();
        let content_type =
            response.headers().get("content-type").and_then(|t| t.to_str().ok()).unwrap_or_default().to_string();
        let mut response = response;
        let mut body = Vec::new();
        let mut cut = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let room = MAX_BODY_BYTES - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    if chunk.len() >= room {
                        cut = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => return ToolOutput::error(format!("Reading {url} failed: {e:#}")),
            }
        }
        if !status.is_success() {
            let text = decode(&content_type, &body);
            let excerpt: String = text.chars().take(500).collect();
            return ToolOutput::error(format!("{url} answered {status}.\n{excerpt}"));
        }
        let text = match page_text(&content_type, &body) {
            Ok(text) => text,
            Err(e) => return ToolOutput::error(e),
        };
        let note = if cut { format!("\n\n[The page is longer than {} MB; the rest was not read.]", MAX_BODY_BYTES >> 20) } else { String::new() };
        let page = format!("URL: {url}\n\n{}{note}", text.trim());
        ToolOutput::ok(spill::shorten(&page, MAX_OUTPUT_BYTES, NAME))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve() -> String {
        let app = axum::Router::new()
            .route(
                "/page",
                axum::routing::get(|| async {
                    let html = "<html><head><title>T</title><style>p{}</style></head><body><h1>Title</h1>\
                                <script>alert(1)</script><p>Some <a href=\"/x\">link</a>.</p></body></html>";
                    ([("content-type", "text/html; charset=utf-8")], html)
                }),
            )
            .route(
                "/gbk",
                axum::routing::get(|| async {
                    // "<p>中文</p>" in GBK, named by the header.
                    ([("content-type", "text/html; charset=GBK")], b"<p>\xd6\xd0\xce\xc4</p>".to_vec())
                }),
            )
            .route(
                "/sjis",
                axum::routing::get(|| async {
                    // "日本" in Shift_JIS, named only by the page.
                    let page = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=Shift_JIS\">\
                                 </head><body><p>\x93\xfa\x96\x7b</p></body></html>";
                    ([("content-type", "text/html")], page.to_vec())
                }),
            )
            .route("/data", axum::routing::get(|| async { ([("content-type", "application/json")], "{\"a\": 1}") }))
            .route("/image", axum::routing::get(|| async { ([("content-type", "image/png")], vec![0u8; 4]) }))
            .route("/missing", axum::routing::get(|| async { (axum::http::StatusCode::NOT_FOUND, "no such page") }))
            .route("/here", axum::routing::get(|| async { axum::response::Redirect::temporary("/page") }))
            .route("/away", axum::routing::get(|| async { axum::response::Redirect::temporary("https://example.com/") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn fetches_pages_as_markdown() {
        let base = serve().await;
        let tool = WebFetchTool { local: true, client: OnceLock::new() };
        let fetch = |path: &str| tool.run(json!({"url": format!("{base}{path}")}));
        let page = fetch("/page").await;
        assert!(!page.is_error, "{}", page.content);
        assert!(page.content.contains("# Title") && page.content.contains("[link](/x)"), "{}", page.content);
        assert!(page.content.contains("Title: T\n"), "the page's title: {}", page.content);
        assert!(!page.content.contains("alert") && !page.content.contains("p{}"), "{}", page.content);
        assert!(fetch("/gbk").await.content.ends_with("中文"), "the header's charset");
        let sjis = fetch("/sjis").await;
        assert!(sjis.content.ends_with("日本"), "the page's own charset: {}", sjis.content);
        assert!(fetch("/data").await.content.ends_with("{\"a\": 1}"));
        assert!(fetch("/image").await.content.contains("image/png"));
        let missing = fetch("/missing").await;
        assert!(missing.is_error && missing.content.contains("404") && missing.content.contains("no such page"));
        assert!(fetch("/here").await.content.contains("# Title"), "followed on the same host");
        let away = fetch("/away").await;
        assert!(!away.is_error && away.content.contains("redirects to https://example.com/"), "{}", away.content);
    }

    #[test]
    fn decodes_the_charset_named() {
        assert_eq!(decode("text/plain; charset=\"iso-8859-1\"", b"caf\xe9"), "café");
        assert_eq!(decode("text/plain", b"\xef\xbb\xbfok"), "ok", "a byte-order mark");
        assert_eq!(decode("text/plain; charset=gbk", b"\xef\xbb\xbfok"), "ok", "the mark goes first");
        assert_eq!(decode("application/xml", b"<?xml version=\"1.0\" encoding=\"GB2312\"?><a>\xd6\xd0</a>"), "<?xml version=\"1.0\" encoding=\"GB2312\"?><a>中</a>");
        assert_eq!(decode("text/html", b"<meta charset=utf-16><p>x</p>"), "<meta charset=utf-16><p>x</p>", "UTF-16 named in ASCII is not");
        assert_eq!(decode("text/html; charset=nonsense", "é".as_bytes()), "é");
        let json = "{\"note\": \"<meta charset=gbk>\", \"name\": \"é\"}";
        assert_eq!(decode("application/json", json.as_bytes()), json, "only HTML and XML name their own");
        let html = "<form accept-charset=\"latin1\"><p>é</p></form>";
        assert_eq!(decode("text/html", html.as_bytes()), html, "only a <meta>");
    }

    #[test]
    fn fetches_public_pages_only() {
        let refused = |url: &str| refused(&Url::parse(url).unwrap(), false);
        for local in [
            "http://localhost:8080/",
            "http://localhost./",
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://100.100.1.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fe80::1]/",
            "http://intranet/",
            "http://printer.local/",
        ] {
            assert!(refused(local).is_some(), "{local}");
        }
        assert!(refused("https://docs.rs/serde").is_none());
        assert!(refused("https://8.8.8.8/").is_none());
        assert!(refused("ftp://example.com/").unwrap().contains("Only http"));
        assert!(same_site(&Url::parse("https://example.com/a").unwrap(), &Url::parse("https://www.example.com/b").unwrap()));
        assert!(!same_site(&Url::parse("https://example.com/").unwrap(), &Url::parse("https://evil.com/").unwrap()));
    }
}
