// Fake HTTP servers for tests (like dev/verify-digest.mjs): record requests, answer with a closure.
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    /// Path with query string.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
    pub fn query(&self, key: &str) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        q.split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| form_decode(v))
    }
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

pub fn json(status: u16, body: Value) -> Resp {
    Resp {
        status,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    }
}

pub fn raw(status: u16, content_type: &'static str, body: &[u8]) -> Resp {
    Resp {
        status,
        content_type,
        body: body.to_vec(),
    }
}

pub type Handler = dyn Fn(&Req) -> Resp + Send + Sync;

pub struct Server {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    pub fn requests(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }
}

pub async fn serve(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler: Arc<Handler> = Arc::new(handler);
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let (handler, log) = (handler.clone(), log.clone());
            tokio::spawn(async move {
                if let Some(req) = read_request(s).await {
                    let (mut s, req) = req;
                    let resp = handler(&req);
                    log.lock().unwrap().push(req);
                    let head = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        resp.status,
                        resp.content_type,
                        resp.body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&resp.body).await;
                    let _ = s.shutdown().await;
                }
            });
        }
    });
    Server { url, seen }
}

async fn read_request(mut s: TcpStream) -> Option<(TcpStream, Req)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((
        s,
        Req {
            method,
            target,
            headers,
            body,
        },
    ))
}
