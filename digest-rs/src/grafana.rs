// Port of digest/grafana.mjs (auth, links and response parsing; HTTP calls come with the report step).
use serde_json::{Value, json};

use std::time::Duration;

use serde_json::Map;

use crate::config::Config;
use crate::http::{DEFAULT_TIMEOUT, FetchError, get_json, parse_url, read_body, res_json, send};
use crate::js::{
    JsMap, encode_uri_component, get, iso_string, js_normalize, nullish, num_value,
    number_to_string, search_params, to_js_string, to_number, truthy,
};
use crate::report::{AlertsData, Image, Images, Snapshot};

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

fn grafana_base(cfg: &Config) -> &str {
    cfg.grafana_url
        .strip_suffix('/')
        .unwrap_or(&cfg.grafana_url)
}

async fn api(client: &reqwest::Client, cfg: &Config, path: &str) -> Result<Value, FetchError> {
    get_json(
        client,
        &format!("{}{path}", grafana_base(cfg)),
        &auth_headers(cfg),
        DEFAULT_TIMEOUT,
    )
    .await
}

pub async fn collect_alerts(
    client: &reqwest::Client,
    cfg: &Config,
    from_ms: i64,
    to_ms: i64,
) -> Result<AlertsData, FetchError> {
    let rules = summarize_rules(&api(client, cfg, "/api/prometheus/grafana/api/v1/rules").await?)?;
    let (from, to) = (from_ms.to_string(), to_ms.to_string());
    let qs = search_params(&[
        ("type", "alert"),
        ("from", &from),
        ("to", &to),
        ("limit", "200"),
    ]);
    // history is best effort
    let fired = match api(client, cfg, &format!("/api/annotations?{qs}")).await {
        Ok(list) => fired_from_annotations(&list).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Ok(AlertsData { rules, fired })
}

pub async fn collect_deploys(
    client: &reqwest::Client,
    cfg: &Config,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<Deploy>, FetchError> {
    let (from, to) = (from_ms.to_string(), to_ms.to_string());
    let qs = search_params(&[
        ("tags", "deploy"),
        ("from", &from),
        ("to", &to),
        ("limit", "20"),
    ]);
    parse_deploys(&api(client, cfg, &format!("/api/annotations?{qs}")).await?)
}

/// `str.replaceAll(search, replacement)` with a string replacement (`$$`, `$&`, `` $` ``, `$'` are expanded).
fn replace_all_js(s: &str, search: &str, replacement: &str) -> String {
    if search.is_empty() {
        return s.to_string(); // never happens: the search always starts with `$`
    }
    let mut out = String::new();
    let mut last = 0;
    for (pos, m) in s.match_indices(search) {
        out.push_str(&s[last..pos]);
        let mut chars = replacement.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                out.push(c);
                continue;
            }
            match chars.peek() {
                Some('$') => out.push('$'),
                Some('&') => out.push_str(m),
                Some('`') => out.push_str(&s[..pos]),
                Some('\'') => out.push_str(&s[pos + m.len()..]),
                _ => {
                    out.push('$');
                    continue;
                }
            }
            chars.next();
        }
        last = pos + m.len();
    }
    out.push_str(&s[last..]);
    out
}

/// Replace `${var}` / `$var` with the variable's "All" value (`.*` by default).
fn fill_vars(expr: Option<&Value>, dashboard: &Value) -> Result<String, FetchError> {
    let mut out = nullish(expr).map_or_else(String::new, |e| to_js_string(Some(e)));
    let list = nullish(get(dashboard, "templating")).and_then(|t| get(t, "list"));
    for v in items(list)? {
        let v = not_null(v)?;
        let all = get(v, "allValue")
            .filter(|a| truthy(Some(a)))
            .map_or_else(|| ".*".to_string(), |a| to_js_string(Some(a)));
        let name = to_js_string(get(v, "name"));
        out = replace_all_js(&out, &format!("${{{name}}}"), &all);
        out = replace_all_js(&out, &format!("${name}"), &all);
    }
    Ok(out)
}

fn post_json(
    client: &reqwest::Client,
    cfg: &Config,
    path: &str,
    body: &Value,
) -> Result<reqwest::RequestBuilder, FetchError> {
    let mut req = client
        .post(parse_url(&format!("{}{path}", grafana_base(cfg)))?)
        .timeout(Duration::from_millis(30_000));
    for (k, v) in auth_headers(cfg) {
        req = req.header(k, v);
    }
    Ok(req
        .header("Content-Type", "application/json")
        .body(body.to_string()))
}

/// `Object.values(x)` for a JSON value: object values, array elements, nothing otherwise.
// shortcut: JS also yields the characters of a string here; Grafana never sends one.
fn object_values(v: &Value) -> Vec<&Value> {
    match v {
        Value::Object(m) => m.values().collect(),
        Value::Array(a) => a.iter().collect(),
        _ => Vec::new(),
    }
}

/// Local (non-external) snapshot of the dashboard for the window, with data queried from the panels' own datasource.
pub async fn create_snapshot(
    client: &reqwest::Client,
    cfg: &Config,
    from_ms: i64,
    to_ms: i64,
) -> Result<Snapshot, FetchError> {
    let body = api(
        client,
        cfg,
        &format!(
            "/api/dashboards/uid/{}",
            encode_uri_component(&cfg.dashboard_uid)
        ),
    )
    .await?;
    let dashboard = get(not_null(&body)?, "dashboard")
        .cloned()
        .filter(|d| truthy(Some(d)))
        .ok_or_else(|| FetchError::source("대시보드 없음"))?;
    // shortcut: a truthy non-object dashboard makes JS throw on assignment; same TypeError here.
    let Value::Object(_) = dashboard else {
        return Err(FetchError::type_error(None));
    };
    let mut dashboard = dashboard;
    js_normalize(&mut dashboard);
    let interval_ms = 15_000f64.max(((to_ms - from_ms) as f64 / 300.0).ceil());
    let panel_count = items(get(&dashboard, "panels"))?.len();
    for i in 0..panel_count {
        let panel = &dashboard["panels"][i];
        let panel = not_null(panel)?;
        let targets = get(panel, "targets");
        let has_targets = match nullish(targets) {
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::String(s)) => !s.is_empty(), // `.length` of a string; `.filter` then throws below
            _ => false,
        };
        if get(panel, "type") == Some(&json!("row")) || !has_targets {
            continue;
        }
        let Some(Value::Array(targets)) = targets else {
            return Err(FetchError::type_error(None));
        };
        let mut queries = Vec::new();
        for t in targets {
            let t = not_null(t)?;
            if !truthy(get(t, "expr")) {
                continue;
            }
            let mut q = Map::new();
            let mut put = |k: &str, v: Option<&Value>| {
                if let Some(v) = v {
                    q.insert(k.to_string(), v.clone());
                }
            };
            put("refId", get(t, "refId"));
            put(
                "datasource",
                nullish(get(t, "datasource")).or_else(|| get(panel, "datasource")),
            );
            put("expr", Some(&json!(fill_vars(get(t, "expr"), &dashboard)?)));
            put("legendFormat", get(t, "legendFormat"));
            put("range", Some(&json!(true)));
            put("instant", Some(&json!(false)));
            put("intervalMs", Some(&num_value(interval_ms)));
            put("maxDataPoints", Some(&json!(300)));
            queries.push(Value::Object(q));
        }
        if queries.is_empty() {
            continue;
        }
        let req = post_json(
            client,
            cfg,
            "/api/ds/query",
            &json!({"queries": queries, "from": from_ms.to_string(), "to": to_ms.to_string()}),
        )?;
        let res = send(req).await?;
        if !res.status().is_success() {
            continue; // panel stays empty rather than failing the whole snapshot
        }
        let body = res_json(res).await?;
        let mut frames = Vec::new();
        for r in object_values(
            &nullish(get(not_null(&body)?, "results"))
                .cloned()
                .unwrap_or_else(|| json!({})),
        ) {
            match nullish(get(not_null(r)?, "frames")) {
                Some(Value::Array(a)) => frames.extend(a.iter().cloned()),
                Some(other) => frames.push(other.clone()),
                None => {}
            }
        }
        if let Some(Value::Object(p)) = dashboard.get_mut("panels").and_then(|p| p.get_mut(i)) {
            p.insert("snapshotData".to_string(), Value::Array(frames));
            p.insert("targets".to_string(), json!([]));
        }
    }
    if let Value::Object(d) = &mut dashboard {
        d.insert(
            "time".to_string(),
            json!({"from": iso_string(from_ms), "to": iso_string(to_ms)}),
        );
        let mut picker = match d.get("timepicker") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        picker.insert("hidden".to_string(), json!(true));
        d.insert("timepicker".to_string(), Value::Object(picker));
    }
    let name = format!("digest {}", &iso_string(to_ms)[..10]);
    // external stays false: the snapshot lives on this Grafana, never on snapshots.raintank.io
    let payload = json!({"dashboard": dashboard, "name": name, "expires": num_value(cfg.snapshot_expires_sec), "external": false});
    let res = send(post_json(client, cfg, "/api/snapshots", &payload)?).await?;
    if !res.status().is_success() {
        return Err(FetchError::Source(format!(
            "HTTP {}",
            res.status().as_u16()
        )));
    }
    let body = res_json(res).await?;
    let key = get(not_null(&body)?, "key")
        .filter(|k| truthy(Some(k)))
        .ok_or_else(|| FetchError::source("응답 형식 오류"))?;
    // build the link from the key; the response url uses Grafana's internal app_url
    Ok(Snapshot {
        url: format!(
            "{}/dashboard/snapshot/{}",
            public_base(cfg),
            encode_uri_component(&to_js_string(Some(key)))
        ),
    })
}

async fn render_one(
    client: &reqwest::Client,
    cfg: &Config,
    id: f64,
    from_ms: i64,
    to_ms: i64,
) -> Result<Image, FetchError> {
    let (panel, from, to) = (number_to_string(id), from_ms.to_string(), to_ms.to_string());
    let qs = search_params(&[
        ("orgId", "1"),
        ("panelId", &panel),
        ("from", &from),
        ("to", &to),
        ("width", "900"),
        ("height", "380"),
        ("theme", "light"),
        ("tz", &cfg.tz),
    ]);
    let uid = encode_uri_component(&cfg.dashboard_uid);
    let url = format!("{}/render/d-solo/{uid}/{uid}?{qs}", grafana_base(cfg));
    let mut req = client
        .get(parse_url(&url)?)
        .timeout(Duration::from_millis(60_000));
    for (k, v) in auth_headers(cfg) {
        req = req.header(k, v);
    }
    let res = send(req).await?;
    if !res.status().is_success() {
        return Err(FetchError::Source(format!(
            "HTTP {}",
            res.status().as_u16()
        )));
    }
    let content_type = res
        .headers()
        .get("content-type")
        .map_or("null".to_string(), |v| {
            String::from_utf8_lossy(v.as_bytes()).into_owned()
        });
    if !content_type.starts_with("image/png") {
        return Err(FetchError::source("PNG 아님"));
    }
    Ok(Image {
        panel_id: id,
        name: format!("panel-{panel}.png"),
        buffer: read_body(res).await?,
    })
}

/// Panel PNGs, one after another. Fails only when every panel failed.
pub async fn render_panels(
    client: &reqwest::Client,
    cfg: &Config,
    from_ms: i64,
    to_ms: i64,
) -> Result<Images, FetchError> {
    let mut images = Vec::new();
    let mut failures = Vec::new();
    for &id in &cfg.panel_ids {
        match render_one(client, cfg, id, from_ms, to_ms).await {
            Ok(img) => images.push(img),
            Err(e) => failures.push(format!("panel {}: {e}", number_to_string(id))),
        }
    }
    match failures.first() {
        Some(first) if images.is_empty() => Err(FetchError::Source(first.clone())),
        _ => Ok(Images { images, failures }),
    }
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
