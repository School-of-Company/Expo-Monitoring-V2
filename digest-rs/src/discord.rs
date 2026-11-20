// Port of digest/discord.mjs: embed limits and splitting, webhook POST with retries.
use std::future::Future;
use std::time::Duration;

use serde::Serialize;

use crate::http::{classify, parse_url};
use crate::mask::{LONE_SURROGATE_BASE, truncate};
use crate::report::{Field, Image};

pub const LIMIT_TITLE: usize = 256;
pub const LIMIT_DESCRIPTION: usize = 4096;
pub const LIMIT_FIELD_NAME: usize = 256;
pub const LIMIT_FIELD_VALUE: usize = 1024;
pub const LIMIT_FIELDS: usize = 25;
pub const LIMIT_FOOTER: usize = 2048;
pub const LIMIT_TOTAL: usize = 6000;
pub const LIMIT_EMBEDS_PER_MESSAGE: usize = 10;
const MAX_MESSAGES: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Footer {
    pub text: String,
}

/// Field order is the JSON key order of the JS object (footer is always last).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextEmbed {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub color: u32,
    pub fields: Vec<EmbedField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<Footer>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Embed {
    Text(TextEmbed),
    Image { image: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub embeds: Vec<Embed>,
    pub images: Vec<String>,
}

/// JS string `.length`.
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn size_of_field(f: &EmbedField) -> usize {
    js_len(&f.name) + js_len(&f.value)
}

pub fn text_size(e: &TextEmbed) -> usize {
    js_len(&e.title)
        + e.description.as_deref().map_or(0, js_len)
        + e.footer.as_ref().map_or(0, |f| js_len(&f.text))
        + e.fields.iter().map(size_of_field).sum::<usize>()
}

pub struct Digest<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub color: u32,
    pub footer: &'a str,
    pub fields: &'a [Field],
    pub image_names: &'a [String],
}

/// Lay out one digest into Discord messages that respect every embed limit.
/// `fields` must already be in priority order; lower-priority fields spill into
/// continuation messages, and anything past MAX_MESSAGES is dropped with a note.
/// Image embeds (no text) ride on the first message.
pub fn build_messages(d: &Digest) -> Vec<Message> {
    let or_dash = |s: &str| {
        if s.is_empty() {
            "-".to_string()
        } else {
            s.to_string()
        }
    };
    let clean: Vec<EmbedField> = d
        .fields
        .iter()
        .map(|f| EmbedField {
            name: truncate(&or_dash(&f.name), LIMIT_FIELD_NAME),
            value: truncate(&or_dash(&f.value), LIMIT_FIELD_VALUE),
            inline: false,
        })
        .collect();
    let head = |t: &str, desc: &str| TextEmbed {
        title: truncate(t, LIMIT_TITLE),
        description: (!desc.is_empty()).then(|| truncate(desc, LIMIT_DESCRIPTION)),
        color: d.color,
        fields: Vec::new(),
        footer: (!d.footer.is_empty()).then(|| Footer {
            text: truncate(d.footer, LIMIT_FOOTER),
        }),
    };
    let mut embeds = vec![head(d.title, d.description)];
    let mut dropped = 0;
    for f in clean {
        let Some(cur) = embeds.last() else { break };
        if cur.fields.len() >= LIMIT_FIELDS || text_size(cur) + size_of_field(&f) > LIMIT_TOTAL {
            if embeds.len() >= MAX_MESSAGES {
                dropped += 1;
                continue;
            }
            embeds.push(head(&format!("{} (계속)", truncate(d.title, 200)), ""));
        }
        if let Some(cur) = embeds.last_mut() {
            cur.fields.push(f);
        }
    }
    if dropped > 0
        && let Some(last) = embeds.last_mut()
    {
        let prev = last.footer.as_ref().map_or("", |f| f.text.as_str());
        let text = format!("{prev} · 낮은 우선순위 {dropped}개 항목 생략");
        last.footer = Some(Footer {
            text: truncate(crate::js::js_trim(&text), LIMIT_FOOTER),
        });
    }
    let mut messages: Vec<Message> = embeds
        .into_iter()
        .map(|e| Message {
            embeds: vec![Embed::Text(e)],
            images: Vec::new(),
        })
        .collect();
    if let Some(first) = messages.first_mut() {
        for name in d.image_names.iter().take(LIMIT_EMBEDS_PER_MESSAGE - 1) {
            first.embeds.push(Embed::Image {
                image: ImageUrl {
                    url: format!("attachment://{name}"),
                },
            });
            first.images.push(name.clone());
        }
    }
    messages
}

/// `JSON.stringify(value, null, indent)`: a string cut by `truncate` keeps its lone surrogate as `\udXXX`.
pub fn to_js_json<T: Serialize>(value: &T, pretty: bool) -> String {
    let s = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .unwrap_or_default();
    s.chars()
        .map(|c| match u32::from(c) {
            u @ LONE_SURROGATE_BASE..=0xE3FF => {
                format!("\\u{:04x}", u - LONE_SURROGATE_BASE + 0xD800)
            }
            _ => c.to_string(),
        })
        .collect()
}

/// Retry-After seconds -> ms, capped at 30s; garbage -> 1s. A missing header is `Number(null)` = 0 in JS.
pub fn retry_delay_ms(header: Option<&str>) -> u64 {
    let n = header.map_or(0.0, crate::js::js_number);
    if n.is_finite() && n >= 0.0 {
        (n.min(30.0) * 1000.0) as u64
    } else {
        1000
    }
}

#[derive(Serialize)]
struct Attachment<'a> {
    id: usize,
    filename: &'a str,
}

#[derive(Serialize)]
struct Payload<'a> {
    username: &'static str,
    allowed_mentions: serde_json::Value,
    embeds: &'a [Embed],
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<Attachment<'a>>>,
}

/// POST one message (multipart when it has images). Retries only 429 (Retry-After)
/// and 5xx / network errors, at most 2 times. Errors never include the webhook URL.
pub async fn post_message<S, F>(
    client: &reqwest::Client,
    webhook_url: &str,
    message: &Message,
    files: &[Image],
    sleep: &S,
) -> Result<(), String>
where
    S: Fn(u64) -> F,
    F: Future<Output = ()>,
{
    let payload = |attachments| Payload {
        username: "Expo Digest",
        allowed_mentions: serde_json::json!({"parse": []}),
        embeds: &message.embeds,
        attachments,
    };
    let mut last_reason = String::new();
    for attempt in 0..=2u64 {
        let mut wait = 1000 * (attempt + 1);
        let sent = match parse_url(webhook_url) {
            Err(e) => Err(e),
            Ok(url) => {
                let req = client.post(url).timeout(Duration::from_millis(30_000));
                let req = if message.images.is_empty() {
                    req.header("Content-Type", "application/json")
                        .body(to_js_json(&payload(None), false))
                } else {
                    let attachments = message
                        .images
                        .iter()
                        .enumerate()
                        .map(|(id, filename)| Attachment { id, filename })
                        .collect();
                    let mut form = reqwest::multipart::Form::new().text(
                        "payload_json",
                        to_js_json(&payload(Some(attachments)), false),
                    );
                    for (id, name) in message.images.iter().enumerate() {
                        // `files[name]` missing in JS becomes the text "undefined"
                        let bytes = files
                            .iter()
                            .find(|i| &i.name == name)
                            .map_or_else(|| b"undefined".to_vec(), |i| i.buffer.clone());
                        let part = reqwest::multipart::Part::bytes(bytes)
                            .file_name(name.clone())
                            .mime_str("image/png")
                            .map_err(|e| e.to_string())?;
                        form = form.part(format!("files[{id}]"), part);
                    }
                    req.multipart(form)
                };
                req.send().await.map_err(|e| classify(&e))
            }
        };
        match sent {
            Ok(res) if res.status().is_success() => return Ok(()),
            Ok(res) => {
                let status = res.status().as_u16();
                last_reason = format!("HTTP {status}");
                if status == 429 {
                    wait = retry_delay_ms(
                        res.headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok()),
                    );
                } else if status < 500 {
                    return Err(format!("Discord 전송 실패 ({last_reason})"));
                }
            }
            Err(e) => last_reason = format!("네트워크 오류({})", e.code_or_name()),
        }
        if attempt < 2 {
            sleep(wait).await;
        }
    }
    Err(format!("Discord 전송 실패 ({last_reason}, 재시도 소진)"))
}

pub async fn real_sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

pub async fn send_all<S, F>(
    client: &reqwest::Client,
    webhook_url: &str,
    messages: &[Message],
    files: &[Image],
    sleep: &S,
) -> Result<(), String>
where
    S: Fn(u64) -> F,
    F: Future<Output = ()>,
{
    for m in messages {
        post_message(client, webhook_url, m, files, sleep).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Req, Resp, raw, serve};
    use std::sync::{Arc, Mutex};

    fn fields(n: usize, value_len: usize) -> Vec<Field> {
        (0..n)
            .map(|i| Field {
                name: format!("f{i}"),
                value: "v".repeat(value_len),
            })
            .collect()
    }
    fn build(title: &str, description: &str, fields: &[Field], images: &[String]) -> Vec<Message> {
        build_messages(&Digest {
            title,
            description,
            color: 1,
            footer: "",
            fields,
            image_names: images,
        })
    }
    fn text(e: &Embed) -> &TextEmbed {
        match e {
            Embed::Text(t) => t,
            Embed::Image { .. } => panic!("image embed"),
        }
    }

    fn assert_limits(messages: &[Message]) {
        for m in messages {
            assert!(m.embeds.len() <= LIMIT_EMBEDS_PER_MESSAGE);
            let total: usize = m
                .embeds
                .iter()
                .map(|e| {
                    if let Embed::Text(t) = e {
                        text_size(t)
                    } else {
                        0
                    }
                })
                .sum();
            assert!(total <= LIMIT_TOTAL, "message total");
            for e in m.embeds.iter().filter_map(|e| {
                if let Embed::Text(t) = e {
                    Some(t)
                } else {
                    None
                }
            }) {
                assert!(js_len(&e.title) <= 256);
                assert!(e.description.as_deref().map_or(0, js_len) <= 4096);
                assert!(e.fields.len() <= 25);
                for f in &e.fields {
                    assert!(js_len(&f.name) <= 256 && js_len(&f.value) <= 1024);
                    assert!(!f.value.is_empty());
                }
            }
        }
    }

    // ---- discord.test.mjs ----
    #[test]
    fn truncates_oversized_strings() {
        let m = build(
            &"x".repeat(500),
            &"y".repeat(9000),
            &[Field {
                name: "n".repeat(400),
                value: "v".repeat(3000),
            }],
            &[],
        );
        assert_limits(&m);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn splits_across_messages_by_priority_keeps_order() {
        let m = build("t", &"d".repeat(4000), &fields(30, 1000), &[]);
        assert_limits(&m);
        assert!((2..=3).contains(&m.len()));
        assert_eq!(text(&m[0].embeds[0]).fields[0].name, "f0");
        let flat: Vec<usize> = m
            .iter()
            .flat_map(|x| {
                text(&x.embeds[0])
                    .fields
                    .iter()
                    .map(|f| f.name[1..].parse::<usize>().unwrap())
            })
            .collect();
        assert!(flat.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn drops_lowest_priority_beyond_3_messages_and_says_so() {
        let m = build("t", "d", &fields(100, 1000), &[]);
        assert_limits(&m);
        assert_eq!(m.len(), 3);
        assert!(
            text(&m[2].embeds[0])
                .footer
                .as_ref()
                .unwrap()
                .text
                .contains("생략")
        );
    }

    #[test]
    fn field_cap_25_per_embed() {
        let f: Vec<Field> = (0..40)
            .map(|_| Field {
                name: "a".into(),
                value: "b".into(),
            })
            .collect();
        let m = build("t", "d", &f, &[]);
        assert_limits(&m);
        assert_eq!(text(&m[0].embeds[0]).fields.len(), 25);
    }

    #[test]
    fn images_attach_via_attachment_on_first_message() {
        let m = build("t", "d", &[], &["a.png".into(), "b.png".into()]);
        assert_eq!(m[0].images, vec!["a.png", "b.png"]);
        assert_eq!(
            m[0].embeds[1],
            Embed::Image {
                image: ImageUrl {
                    url: "attachment://a.png".into()
                }
            }
        );
    }

    fn webhook(statuses: Vec<(u16, Option<&'static str>)>) -> impl Fn(&Req) -> Resp {
        let seq = Arc::new(Mutex::new(statuses.into_iter()));
        move |_| {
            let (status, retry) = seq.lock().unwrap().next().unwrap_or((204, None));
            let mut r = raw(status, "application/json", b"{}");
            if let Some(v) = retry {
                r.content_type =
                    Box::leak(format!("application/json\r\nretry-after: {v}").into_boxed_str());
            }
            r
        }
    }

    fn msg() -> Message {
        Message {
            embeds: vec![Embed::Text(TextEmbed {
                title: "t".into(),
                description: None,
                color: 1,
                fields: vec![],
                footer: None,
            })],
            images: vec![],
        }
    }

    async fn post(url: &str, m: &Message, files: &[Image]) -> (Result<(), String>, Vec<u64>) {
        let waits = Mutex::new(Vec::new());
        let sleep = |ms: u64| {
            waits.lock().unwrap().push(ms);
            async {}
        };
        let r = post_message(&crate::http::client().unwrap(), url, m, files, &sleep).await;
        (r, waits.into_inner().unwrap())
    }

    #[tokio::test]
    async fn retry_429_honours_retry_after_then_succeeds() {
        let s = serve(webhook(vec![(429, Some("2"))])).await;
        let (r, waits) = post(&format!("{}/secret-token", s.url), &msg(), &[]).await;
        assert_eq!(r, Ok(()));
        assert_eq!(waits, vec![2000]);
        assert_eq!(retry_delay_ms(Some("999")), 30000);
    }

    #[tokio::test]
    async fn retry_5xx_up_to_2_retries_then_fail_without_leaking_the_url() {
        let s = serve(webhook(vec![(502, None), (502, None), (502, None)])).await;
        let (r, _) = post(&format!("{}/secret-token", s.url), &msg(), &[]).await;
        let e = r.unwrap_err();
        assert!(!e.contains("secret-token") && !e.contains("127.0.0.1"));
        assert!(e.contains("502"));
        assert_eq!(s.requests().len(), 3);
    }

    #[tokio::test]
    async fn fails_4xx_immediately_and_retries_network_errors() {
        let s = serve(webhook(vec![(400, None)])).await;
        let (r, _) = post(&format!("{}/x", s.url), &msg(), &[]).await;
        assert!(r.unwrap_err().contains("400"));
        assert_eq!(s.requests().len(), 1);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let (r, waits) = post(&format!("http://127.0.0.1:{port}/secret"), &msg(), &[]).await;
        assert_eq!(
            r,
            Err("Discord 전송 실패 (네트워크 오류(ECONNREFUSED), 재시도 소진)".to_string())
        );
        assert_eq!(waits, vec![1000, 2000]);
    }

    #[tokio::test]
    async fn multipart_body_carries_payload_json_and_files() {
        let s = serve(webhook(vec![])).await;
        let m = Message {
            embeds: vec![Embed::Image {
                image: ImageUrl {
                    url: "attachment://p.png".into(),
                },
            }],
            images: vec!["p.png".into()],
        };
        let files = [Image {
            panel_id: 1.0,
            name: "p.png".into(),
            buffer: vec![137, 80, 78, 71],
        }];
        assert_eq!(post(&format!("{}/x", s.url), &m, &files).await.0, Ok(()));
        let req = &s.requests()[0];
        assert!(
            req.header("content-type")
                .unwrap()
                .starts_with("multipart/form-data; boundary=")
        );
        let body = String::from_utf8_lossy(&req.body);
        assert!(body.contains(r#"name="payload_json""#));
        assert!(body.contains(r#""attachments":[{"id":0,"filename":"p.png"}]"#));
        assert!(
            body.contains(r#"name="files[0]"; filename="p.png""#) && body.contains("image/png")
        );
        assert!(req.body.windows(4).any(|w| w == [137, 80, 78, 71]));
    }

    // ---- edges, expected values from Node ----
    #[test]
    fn retry_delay_like_number_of_header() {
        let got: Vec<u64> = [
            None,
            Some(""),
            Some("abc"),
            Some("1.5"),
            Some("-1"),
            Some("999"),
            Some(" 2 "),
        ]
        .iter()
        .map(|h| retry_delay_ms(*h))
        .collect();
        assert_eq!(got, vec![0, 0, 1000, 1500, 1000, 30000, 2000]);
    }

    #[test]
    fn json_like_js() {
        let m = build(
            "t",
            "",
            &[Field {
                name: String::new(),
                value: String::new(),
            }],
            &[],
        );
        assert_eq!(
            to_js_json(&m, false),
            r#"[{"embeds":[{"title":"t","color":1,"fields":[{"name":"-","value":"-","inline":false}]}],"images":[]}]"#
        );
        assert_eq!(to_js_json(&truncate("a😀b", 3), false), r#""a\ud83d…""#);
    }
}
