// Port of digest/http.mjs. Errors carry one-line causes only - never URLs, headers or bodies.
//
// `FetchError` mirrors what a Node `fetch` rejection looks like after `describeError`:
// the error name (`TypeError`, `TimeoutError`) and undici's `cause.code` (ECONNREFUSED, ENOTFOUND...).
// The mapping was taken from Node's real behaviour for each failure (refused, DNS, reset, timeout, bad URL...).
use std::fmt;
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(15_000);
/// undici's default connect timeout; it fires before a longer request timeout does.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(10_000);

#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    /// JS `SourceError`: the message is already the one-line cause (`HTTP 500`, `응답 형식 오류`).
    Source(String),
    /// A rejected `fetch`: `name` is the JS error name, `code` undici's `cause.code` when there is one.
    Network {
        name: &'static str,
        code: Option<String>,
    },
}

impl FetchError {
    pub fn source(msg: impl Into<String>) -> Self {
        FetchError::Source(msg.into())
    }
    pub fn type_error(code: Option<&str>) -> Self {
        FetchError::Network {
            name: "TypeError",
            code: code.map(str::to_string),
        }
    }
    fn timeout() -> Self {
        FetchError::Network {
            name: "TimeoutError",
            code: None,
        }
    }

    /// Discord's retry reason: `e.cause.code ?? e.code ?? e.name`.
    pub fn code_or_name(&self) -> String {
        match self {
            FetchError::Source(_) => "Error".to_string(),
            FetchError::Network { name, code } => code.clone().unwrap_or_else(|| name.to_string()),
        }
    }
}

/// JS `describeError`.
impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Source(m) => f.write_str(m),
            FetchError::Network {
                name: "TimeoutError" | "AbortError",
                ..
            } => f.write_str("시간 초과"),
            FetchError::Network { code: Some(c), .. } => write!(f, "연결 실패({c})"),
            FetchError::Network {
                name: "TypeError", ..
            } => f.write_str("연결 실패"),
            FetchError::Network { name, .. } => f.write_str(name),
        }
    }
}

impl std::error::Error for FetchError {}

pub fn describe_error(e: &FetchError) -> String {
    e.to_string()
}

/// Replace every occurrence of each secret with `***`.
pub fn scrub(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text.to_string(), |out, s| out.replace(s, "***"))
}

/// One shared client: undici's connect timeout and redirect limit (20).
/// Proxy env vars (HTTP_PROXY/HTTPS_PROXY/ALL_PROXY) are ignored, as Node's fetch does by default.
pub fn client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(20))
        .build()
        .map_err(|_| FetchError::type_error(None))
}

/// Ports `fetch` refuses outright ("bad port", Fetch spec).
const BAD_PORTS: &[u16] = &[
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6679, 6697, 10080,
];

/// The checks `fetch` makes before touching the network.
pub fn parse_url(url: &str) -> Result<Url, FetchError> {
    let u = Url::parse(url).map_err(|_| FetchError::type_error(Some("ERR_INVALID_URL")))?;
    if !matches!(u.scheme(), "http" | "https") || u.port().is_some_and(|p| BAD_PORTS.contains(&p)) {
        return Err(FetchError::type_error(None));
    }
    Ok(u)
}

/// Map a reqwest failure to the error Node's `fetch` would have rejected with.
pub fn classify(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() && !e.is_connect() {
        return FetchError::timeout();
    }
    if e.is_connect() && e.is_timeout() {
        return FetchError::type_error(Some("UND_ERR_CONNECT_TIMEOUT"));
    }
    if e.is_redirect() {
        return FetchError::type_error(None);
    }
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = src {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            let code = match io.kind() {
                ConnectionRefused => Some("ECONNREFUSED"),
                ConnectionReset => Some("ECONNRESET"),
                ConnectionAborted => Some("ECONNABORTED"),
                HostUnreachable => Some("EHOSTUNREACH"),
                NetworkUnreachable => Some("ENETUNREACH"),
                AddrNotAvailable => Some("EADDRNOTAVAIL"),
                TimedOut => Some("ETIMEDOUT"),
                BrokenPipe => Some("EPIPE"),
                _ => None,
            };
            if code.is_some() {
                return FetchError::type_error(code);
            }
        }
        // shortcut: every resolver failure becomes ENOTFOUND (Node may say EAI_AGAIN for temporary ones); TLS failures have no code (Node gives OpenSSL codes)
        if err.to_string().contains("dns error") {
            return FetchError::type_error(Some("ENOTFOUND"));
        }
        src = err.source();
    }
    FetchError::type_error(None)
}

/// JS `res.json()`: UTF-8 with replacement, leading BOM stripped.
// shortcut: serde_json rejects what JSON.parse accepts in two rare cases (lone surrogate escapes, numbers beyond f64 like 1e400) and reports them as 응답 형식 오류.
pub fn parse_json(bytes: &[u8]) -> Result<Value, FetchError> {
    let text = String::from_utf8_lossy(bytes);
    serde_json::from_str(text.strip_prefix('\u{FEFF}').unwrap_or(&text))
        .map_err(|_| FetchError::source("응답 형식 오류"))
}

/// JS `getJson`: GET, non-2xx -> `HTTP {status}`, unreadable/invalid body -> `응답 형식 오류`.
pub async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Value, FetchError> {
    let mut req = client.get(parse_url(url)?).timeout(timeout);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let res = req.send().await.map_err(|e| {
        if e.is_builder() {
            FetchError::type_error(None)
        } else {
            classify(&e)
        }
    })?;
    if !res.status().is_success() {
        return Err(FetchError::Source(format!(
            "HTTP {}",
            res.status().as_u16()
        )));
    }
    let body = res
        .bytes()
        .await
        .map_err(|_| FetchError::source("응답 형식 오류"))?;
    parse_json(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Tiny one-behaviour server: `reply` gets each connection after the request head is read.
    async fn serve<F, Fut>(reply: F) -> String
    where
        F: Fn(tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            let reply = std::sync::Arc::new(reply);
            while let Ok((mut s, _)) = l.accept().await {
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    reply(s).await;
                });
            }
        });
        url
    }

    fn respond(
        raw: &'static str,
    ) -> impl Fn(
        tokio::net::TcpStream,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    + Send
    + Sync {
        move |mut s| {
            Box::pin(async move {
                let _ = s.write_all(raw.as_bytes()).await;
                let _ = s.shutdown().await;
            })
        }
    }

    async fn get(url: &str) -> Result<Value, String> {
        get_json(&client().unwrap(), url, &[], Duration::from_millis(1500))
            .await
            .map_err(|e| e.to_string())
    }

    #[test]
    fn describe_matches_node() {
        assert_eq!(FetchError::source("HTTP 500").to_string(), "HTTP 500");
        assert_eq!(FetchError::timeout().to_string(), "시간 초과");
        assert_eq!(
            FetchError::type_error(Some("ECONNREFUSED")).to_string(),
            "연결 실패(ECONNREFUSED)"
        );
        assert_eq!(FetchError::type_error(None).to_string(), "연결 실패");
        assert_eq!(
            FetchError::Network {
                name: "SyntaxError",
                code: None
            }
            .to_string(),
            "SyntaxError"
        );
        assert_eq!(
            FetchError::type_error(Some("ECONNRESET")).code_or_name(),
            "ECONNRESET"
        );
        assert_eq!(FetchError::timeout().code_or_name(), "TimeoutError");
    }

    #[test]
    fn scrub_replaces_every_secret() {
        assert_eq!(
            scrub(
                "a https://h/tok b https://h/tok glsa",
                &["https://h/tok", "glsa"]
            ),
            "a *** b *** ***"
        );
        assert_eq!(scrub("nothing", &[]), "nothing");
    }

    #[test]
    fn json_like_res_json() {
        assert_eq!(
            parse_json("\u{FEFF}{\"a\":1}".as_bytes()),
            Ok(serde_json::json!({"a": 1}))
        );
        assert_eq!(
            parse_json(b"{oops"),
            Err(FetchError::source("응답 형식 오류"))
        );
        // duplicate keys: last value wins, first position kept (like JSON.parse)
        assert_eq!(
            serde_json::to_string(&parse_json(br#"{"a":1,"b":2,"a":3}"#).unwrap()).unwrap(),
            r#"{"a":3,"b":2}"#
        );
    }

    // ---- failure modes; expected causes were observed from Node's getJson ----
    #[tokio::test]
    async fn url_checks_before_network() {
        assert_eq!(
            get("http://[bad/x").await,
            Err("연결 실패(ERR_INVALID_URL)".into())
        );
        assert_eq!(
            get("/api/v1/query").await,
            Err("연결 실패(ERR_INVALID_URL)".into())
        );
        assert_eq!(get("prometheus:9090/api").await, Err("연결 실패".into())); // unknown scheme
        assert_eq!(get("ftp://x/y").await, Err("연결 실패".into()));
        assert_eq!(get("http://127.0.0.1:6000/").await, Err("연결 실패".into())); // bad port
    }

    #[tokio::test]
    async fn refused_and_dns() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert_eq!(
            get(&format!("http://127.0.0.1:{port}/x")).await,
            Err("연결 실패(ECONNREFUSED)".into())
        );
        assert_eq!(
            get("http://no-such-host.invalid/x").await,
            Err("연결 실패(ENOTFOUND)".into())
        );
    }

    #[tokio::test]
    async fn status_body_and_timeouts() {
        let url = serve(respond(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n",
        ))
        .await;
        assert_eq!(get(&url).await, Err("HTTP 503".into()));
        let url = serve(respond("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n{oops")).await;
        assert_eq!(get(&url).await, Err("응답 형식 오류".into()));
        let url = serve(respond(
            "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"a\":",
        ))
        .await; // truncated body
        assert_eq!(get(&url).await, Err("응답 형식 오류".into()));
        let url = serve(respond(
            "HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n{\"a\":1}",
        ))
        .await;
        assert_eq!(get(&url).await, Ok(serde_json::json!({"a": 1})));
        let url = serve(respond("NOT HTTP\r\n\r\n")).await;
        assert_eq!(get(&url).await, Err("연결 실패".into()));
        let url = serve(|s| async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(s);
        })
        .await;
        assert_eq!(get(&url).await, Err("시간 초과".into()));
        let url = serve(|mut s| async move {
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 50\r\n\r\n{\"a\":")
                .await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;
        assert_eq!(get(&url).await, Err("응답 형식 오류".into())); // body stalls past the timeout
        let url = serve(respond(
            "HTTP/1.1 302 Found\r\nlocation: /loop\r\ncontent-length: 0\r\n\r\n",
        ))
        .await;
        assert_eq!(get(&url).await, Err("연결 실패".into())); // redirect count exceeded
    }

    #[tokio::test]
    async fn reset_connection() {
        let url = serve(|s| async move {
            let _ = s.set_zero_linger(); // RST, like Node's socket.destroy() with unread data
            drop(s);
        })
        .await;
        assert_eq!(get(&url).await, Err("연결 실패(ECONNRESET)".into()));
    }
}
