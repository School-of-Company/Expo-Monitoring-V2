// Port of digest/grafana.mjs (auth, links and response parsing; HTTP calls come with the report step).
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::FetchError;
use crate::js::{JsMap, encode_uri_component, get, nullish, to_js_string, to_number};

/// Bearer token first, then Basic auth, else none.
pub fn auth_headers(cfg: &Config) -> Vec<(&'static str, String)> {
    if !cfg.grafana_token.is_empty() {
        return vec![("Authorization", format!("Bearer {}", cfg.grafana_token))];
    }
    if !cfg.grafana_user.is_empty() {
        let raw = format!("{}:{}", cfg.grafana_user, cfg.grafana_password);
        return vec![("Authorization", format!("Basic {}", base64(raw.as_bytes())))];
    }
    Vec::new()
}

/// Standard base64 with padding (`Buffer.toString('base64')`).
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(char::from(T[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `(GRAFANA_PUBLIC_URL || GRAFANA_URL)` without one trailing slash.
pub fn public_base(cfg: &Config) -> String {
    let base = if cfg.grafana_public_url.is_empty() {
        &cfg.grafana_url
    } else {
        &cfg.grafana_public_url
    };
    base.strip_suffix('/').unwrap_or(base).to_string()
}

pub fn dashboard_link(cfg: &Config, from_ms: i64, to_ms: i64) -> String {
    format!(
        "{}/d/{}?orgId=1&from={from_ms}&to={to_ms}",
        public_base(cfg),
        encode_uri_component(&cfg.dashboard_uid)
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RuleCounts {
    pub normal: u64,
    pub pending: u64,
    pub firing: u64,
    pub error: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FiringRule {
    /// `mask(r.name)` input: `String(r.name ?? '')`.
    pub name: String,
    /// `r.labels?.severity ?? ''`, kept as JSON for JS truthiness and `=== 'critical'`.
    pub severity: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuleSummary {
    pub counts: RuleCounts,
    pub firing: Vec<FiringRule>,
}

/// A non-array that JS would try to iterate: TypeError. null/undefined -> empty.
// shortcut: JS iterates a string char by char; here any non-array is a TypeError.
fn items(v: Option<&Value>) -> Result<&[Value], FetchError> {
    match nullish(v) {
        None => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(FetchError::type_error(None)),
    }
}

fn not_null(v: &Value) -> Result<&Value, FetchError> {
    if v.is_null() {
        Err(FetchError::type_error(None))
    } else {
        Ok(v)
    }
}

/// Summarise Grafana-managed rule states from /api/prometheus/grafana/api/v1/rules.
pub fn summarize_rules(body: &Value) -> Result<RuleSummary, FetchError> {
    let groups = get(body, "data")
        .and_then(|d| get(d, "groups"))
        .and_then(Value::as_array);
    let (true, Some(groups)) = (get(body, "status") == Some(&json!("success")), groups) else {
        return Err(FetchError::source("응답 형식 오류"));
    };
    let mut counts = RuleCounts::default();
    let mut firing = Vec::new();
    let is = |r: &Value, k: &str, v: &str| get(r, k) == Some(&json!(v));
    for g in groups {
        for r in items(get(not_null(g)?, "rules"))? {
            let r = not_null(r)?;
            if is(r, "health", "error") || is(r, "state", "error") {
                counts.error += 1;
            } else if is(r, "state", "firing") || is(r, "state", "alerting") {
                counts.firing += 1;
                let severity = nullish(get(r, "labels"))
                    .and_then(|l| nullish(get(l, "severity")))
                    .cloned()
                    .unwrap_or_else(|| json!(""));
                firing.push(FiringRule {
                    name: nullish(get(r, "name"))
                        .map_or_else(String::new, |n| to_js_string(Some(n))),
                    severity,
                });
            } else if is(r, "state", "pending") {
                counts.pending += 1;
            } else {
                counts.normal += 1;
            }
        }
    }
    Ok(RuleSummary { counts, firing })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    pub name: Value,
    pub times: u64,
}

/// Names of rules that went to Alerting within the window (state-history annotations).
pub fn fired_from_annotations(list: &Value) -> Result<Vec<Fired>, FetchError> {
    let mut names = JsMap::<u64>::default();
    for a in list.as_array().map_or(&[][..], Vec::as_slice) {
        let a = not_null(a)?;
        let state = nullish(get(a, "newState"))
            .or_else(|| nullish(get(a, "text")))
            .map_or_else(String::new, |s| to_js_string(Some(s)));
        let state = state.to_ascii_lowercase(); // the JS /i flag folds ASCII only
        if !(state.contains("alerting") || state.contains("firing")) {
            continue;
        }
        let name = nullish(get(a, "alertName"))
            .or_else(|| nullish(get(a, "title")))
            .cloned()
            .unwrap_or_else(|| json!("?"));
        let times = names.get(&name).copied().unwrap_or(0) + 1;
        names.set(name, times);
    }
    Ok(names
        .entries
        .into_iter()
        .map(|(name, times)| Fired { name, times })
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Deploy {
    /// `a.time` as given (normally epoch ms).
    pub time: Option<Value>,
    pub text: String,
}

/// Deploy annotations, newest first.
pub fn parse_deploys(list: &Value) -> Result<Vec<Deploy>, FetchError> {
    let list = list
        .as_array()
        .ok_or_else(|| FetchError::source("응답 형식 오류"))?;
    let mut out = list
        .iter()
        .map(|a| {
            let a = not_null(a)?;
            let text = nullish(get(a, "text"))
                .or_else(|| nullish(get(a, "title")))
                .map_or_else(String::new, |t| to_js_string(Some(t)));
            Ok(Deploy {
                time: get(a, "time").cloned(),
                text,
            })
        })
        .collect::<Result<Vec<_>, FetchError>>()?;
    // comparator `b.time - a.time`; NaN counts as equal
    out.sort_by(|a, b| {
        let d = to_number(b.time.as_ref()) - to_number(a.time.as_ref());
        d.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        load_config(|k| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        })
    }

    // ---- sources.test.mjs ----
    #[test]
    fn rule_summary_and_annotation_parsing() {
        let s = summarize_rules(&json!({"status": "success", "data": {"groups": [{"rules": [
            {"name": "a", "state": "inactive", "health": "ok"}, {"name": "b", "state": "firing", "health": "ok", "labels": {"severity": "critical"}},
            {"name": "c", "state": "pending", "health": "ok"}, {"name": "d", "state": "inactive", "health": "error"},
        ]}]}}))
        .unwrap();
        assert_eq!(
            s.counts,
            RuleCounts {
                normal: 1,
                pending: 1,
                firing: 1,
                error: 1
            }
        );
        assert_eq!(
            s.firing,
            vec![FiringRule {
                name: "b".into(),
                severity: json!("critical")
            }]
        );
        let fired = fired_from_annotations(
            &json!([{"alertName": "x", "newState": "Alerting"}, {"alertName": "x", "newState": "Alerting"}, {"alertName": "y", "newState": "Normal"}]),
        );
        assert_eq!(
            fired,
            Ok(vec![Fired {
                name: json!("x"),
                times: 2
            }])
        );
    }

    // ---- edges, expected values from Node ----
    #[test]
    fn auth_prefers_bearer_then_basic() {
        assert_eq!(
            auth_headers(&cfg(&[("GRAFANA_TOKEN", "glsa_x"), ("GRAFANA_USER", "u")])),
            vec![("Authorization", "Bearer glsa_x".to_string())]
        );
        assert_eq!(
            auth_headers(&cfg(&[
                ("GRAFANA_USER", "admin"),
                ("GRAFANA_PASSWORD", "pä:ss")
            ])),
            vec![("Authorization", "Basic YWRtaW46cMOkOnNz".to_string())]
        );
        assert_eq!(
            auth_headers(&cfg(&[("GRAFANA_USER", "ab")])),
            vec![("Authorization", "Basic YWI6".to_string())]
        );
        assert!(auth_headers(&cfg(&[])).is_empty());
    }

    #[test]
    fn links() {
        let c = cfg(&[
            ("GRAFANA_URL", "http://grafana:3000/"),
            ("DIGEST_DASHBOARD_UID", "msa overview"),
        ]);
        assert_eq!(
            dashboard_link(&c, 1, 2),
            "http://grafana:3000/d/msa%20overview?orgId=1&from=1&to=2"
        );
        let c = cfg(&[
            ("GRAFANA_URL", "http://g"),
            ("GRAFANA_PUBLIC_URL", "https://pub//"),
        ]);
        assert_eq!(public_base(&c), "https://pub/");
    }

    #[test]
    fn rule_and_annotation_edges() {
        let s = summarize_rules(&json!({"status": "success", "data": {"groups": [{"rules": null}, {}, {"rules": [
            {"state": "Alerting"}, {"state": "alerting", "labels": {"severity": null}}, {"state": "error", "health": "ok"}, {"name": 5, "state": "firing", "labels": "x"},
        ]}]}}))
        .unwrap();
        assert_eq!(
            s.counts,
            RuleCounts {
                normal: 1,
                pending: 0,
                firing: 2,
                error: 1
            }
        );
        assert_eq!(
            s.firing,
            vec![
                FiringRule {
                    name: "".into(),
                    severity: json!("")
                },
                FiringRule {
                    name: "5".into(),
                    severity: json!("")
                }
            ]
        );
        assert_eq!(
            summarize_rules(&json!({"status": "success", "data": {"groups": [null]}})),
            Err(FetchError::type_error(None))
        );
        assert_eq!(
            summarize_rules(&json!({"status": "success", "data": {}})),
            Err(FetchError::source("응답 형식 오류"))
        );
        let fired = fired_from_annotations(&json!([{"title": "t", "text": "now FIRING"}, {"newState": null, "text": "alerting"}, {"alertName": 1, "newState": "firing"}, {"alertName": 1.0, "newState": "firing"}])).unwrap();
        assert_eq!(
            fired,
            vec![
                Fired {
                    name: json!("t"),
                    times: 1
                },
                Fired {
                    name: json!("?"),
                    times: 1
                },
                Fired {
                    name: json!(1),
                    times: 2
                }
            ]
        );
        assert_eq!(fired_from_annotations(&json!({"a": 1})), Ok(vec![]));
    }

    #[test]
    fn deploys_newest_first() {
        let d = parse_deploys(&json!([{"time": 1, "text": "a"}, {"time": 3, "title": "b"}, {"time": 2, "text": null, "title": 7}, {"time": "x"}])).unwrap();
        let got: Vec<&str> = d.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(got, vec!["b", "7", "a", ""]);
        assert_eq!(
            parse_deploys(&json!({})),
            Err(FetchError::source("응답 형식 오류"))
        );
    }
}
