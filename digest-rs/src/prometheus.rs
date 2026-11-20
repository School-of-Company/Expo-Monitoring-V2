// Port of digest/prometheus.mjs (parsing part; the HTTP collection comes with the report step).
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use crate::config::Config;
use crate::http::{DEFAULT_TIMEOUT, FetchError, get_json};
use crate::js::{JsMap, get, index, nullish, search_params, to_js_string, to_number};
use crate::report::{OutboxRow, PromAlert, PromData};

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: Value,
    pub value: f64,
}

fn bad_shape() -> FetchError {
    FetchError::source("응답 형식 오류")
}

/// Instant-vector result -> samples (NaN, ±Inf and non-numeric values dropped).
pub fn parse_vector(body: &Value) -> Result<Vec<Sample>, FetchError> {
    let data = get(body, "data");
    let result = data
        .and_then(|d| get(d, "result"))
        .and_then(Value::as_array);
    let ok = get(body, "status") == Some(&json!("success"))
        && data.and_then(|d| get(d, "resultType")) == Some(&json!("vector"));
    let (true, Some(result)) = (ok, result) else {
        return Err(bad_shape());
    };
    let mut out = Vec::new();
    for r in result {
        if r.is_null() {
            return Err(FetchError::type_error(None)); // r.metric on null
        }
        let labels = nullish(get(r, "metric"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let value = to_number(nullish(get(r, "value")).and_then(|v| index(v, 1)).as_ref());
        if value.is_finite() {
            out.push(Sample { labels, value });
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceRow {
    /// The `service` label as JSON (usually a string); `(unknown)` when absent.
    pub service: Value,
    pub requests: f64,
    pub errors: f64,
    pub err_ratio: Option<f64>,
    pub p95: Option<f64>,
    pub instances: Option<f64>,
    pub instances_up: f64,
}

impl ServiceRow {
    pub fn name(&self) -> String {
        to_js_string(Some(&self.service))
    }
}

fn by_service(rows: &[Sample]) -> JsMap<f64> {
    let mut m = JsMap::default();
    for r in rows {
        let key = nullish(get(&r.labels, "service"))
            .cloned()
            .unwrap_or_else(|| json!("(unknown)"));
        m.set(key, r.value);
    }
    m
}

pub struct Series<'a> {
    pub requests: &'a [Sample],
    pub errors: &'a [Sample],
    pub p95: &'a [Sample],
    pub up_total: &'a [Sample],
    pub up_count: &'a [Sample],
}

/// Merge per-service query results into one row per service (order: first seen in requests, p95, upTotal).
pub fn merge_services(s: &Series) -> Vec<ServiceRow> {
    let req = by_service(s.requests);
    let err = by_service(s.errors);
    let lat = by_service(s.p95);
    let total = by_service(s.up_total);
    let up = by_service(s.up_count);
    let mut names = JsMap::<()>::default();
    for (k, _) in req.entries.iter().chain(&lat.entries).chain(&total.entries) {
        names.set(k.clone(), ());
    }
    names
        .entries
        .into_iter()
        .map(|(service, ())| {
            let requests = req.get(&service).copied().unwrap_or(0.0);
            let errors = err.get(&service).copied().unwrap_or(0.0);
            ServiceRow {
                requests,
                errors,
                err_ratio: (requests > 0.0).then(|| errors / requests),
                p95: lat.get(&service).copied(),
                instances: total.get(&service).copied(),
                instances_up: up.get(&service).copied().unwrap_or(0.0),
                service,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Totals {
    pub requests: f64,
    pub errors: f64,
    pub err_ratio: Option<f64>,
}

pub fn totals_of(services: &[ServiceRow]) -> Totals {
    let requests = services.iter().fold(0.0, |a, s| a + s.requests);
    let errors = services.iter().fold(0.0, |a, s| a + s.errors);
    Totals {
        requests,
        errors,
        err_ratio: (requests > 0.0).then(|| errors / requests),
    }
}

/// Results tagged with completion order -> all values, or the earliest error.
fn first_error<T: Default, const N: usize>(
    rs: [(usize, Result<T, FetchError>); N],
) -> Result<[T; N], FetchError> {
    let first = rs
        .iter()
        .filter_map(|(n, r)| r.as_ref().err().map(|e| (*n, e)))
        .min_by_key(|(n, _)| *n);
    if let Some((_, e)) = first {
        return Err(e.clone());
    }
    Ok(rs.map(|(_, r)| r.unwrap_or_default()))
}

async fn query(
    client: &reqwest::Client,
    base: &str,
    time: &str,
    expr: &str,
) -> Result<Vec<Sample>, FetchError> {
    let url = format!(
        "{base}/api/v1/query?{}",
        search_params(&[("query", expr), ("time", time)])
    );
    parse_vector(&get_json(client, &url, &[], DEFAULT_TIMEOUT).await?)
}

/// JS `collectPrometheus`: 5 required queries (first failure wins, like `Promise.all`), then 4 optional ones.
pub async fn collect(
    client: &reqwest::Client,
    cfg: &Config,
    end_ms: i64,
    window_sec: i64,
) -> Result<PromData, FetchError> {
    let base = cfg
        .prometheus_url
        .strip_suffix('/')
        .unwrap_or(&cfg.prometheus_url);
    let time = crate::report::to_fixed(end_ms as f64 / 1000.0, 3);
    let q = |expr: String| {
        let time = time.clone();
        async move { query(client, base, &time, &expr).await }
    };
    let w = format!("{window_sec}s");
    // Like Promise.all: nothing is cancelled, and the failure that happened first in time wins.
    let order = AtomicUsize::new(0);
    let timed = |expr: String| {
        let (fut, order) = (q(expr), &order);
        async move {
            let r = fut.await;
            (order.fetch_add(1, Ordering::SeqCst), r)
        }
    };
    let (a, b, c, d, e) = tokio::join!(
        timed(format!(
            "sum by (service) (increase(http_server_requests_seconds_count[{w}])) or sum by (service) (increase(gateway_http_requests_total[{w}]))"
        )),
        timed(format!(
            "sum by (service) (increase(http_server_requests_seconds_count{{status=~\"5..\"}}[{w}])) or sum by (service) (increase(gateway_http_requests_total{{status=~\"5..\"}}[{w}]))"
        )),
        timed(format!(
            "histogram_quantile(0.95, sum by (service, le) (rate(http_server_requests_seconds_bucket[{w}]))) or histogram_quantile(0.95, sum by (service, le) (rate(gateway_http_request_duration_seconds_bucket[{w}])))"
        )),
        timed("count by (service) (up{job=\"eureka-services\"})".to_string()),
        timed("count by (service) (up{job=\"eureka-services\"} == 1)".to_string()),
    );
    let [requests, errors, p95, up_total, up_count] = first_error([a, b, c, d, e])?;
    let optional = |expr: String| {
        let q = q(expr);
        async move { q.await.unwrap_or_default() }
    };
    let (outbox, outbox_age, pool_waiting, alerts) = tokio::join!(
        optional("sum by (status) (form_outbox_records)".to_string()),
        optional(format!(
            "max(max_over_time(form_outbox_oldest_pending_age_seconds[{w}]))"
        )),
        optional(format!(
            "max(max_over_time(form_db_pool_connections{{state=\"waiting\"}}[{w}]))"
        )),
        optional(format!(
            "max by (alertname, severity) (max_over_time(ALERTS{{alertstate=\"firing\"}}[{w}]))"
        )),
    );
    let services = merge_services(&Series {
        requests: &requests,
        errors: &errors,
        p95: &p95,
        up_total: &up_total,
        up_count: &up_count,
    });
    let label = |s: &Sample, k: &str, d: &str| {
        nullish(get(&s.labels, k))
            .cloned()
            .unwrap_or_else(|| json!(d))
    };
    Ok(PromData {
        totals: totals_of(&services),
        services,
        outbox: outbox
            .iter()
            .map(|r| OutboxRow {
                status: label(r, "status", ""),
                count: r.value,
            })
            .collect(),
        outbox_age: outbox_age.first().map(|r| r.value),
        pool_waiting: pool_waiting.first().map(|r| r.value),
        alerts: alerts
            .iter()
            .map(|r| PromAlert {
                name: label(r, "alertname", "?"),
                severity: label(r, "severity", ""),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_body(rows: &[(Value, Value)]) -> Value {
        json!({"status": "success", "data": {"resultType": "vector", "result":
            rows.iter().map(|(m, v)| json!({"metric": m, "value": [1, v]})).collect::<Vec<_>>()}})
    }
    fn svc(name: &str, v: f64) -> (Value, Value) {
        (
            json!({"service": name}),
            json!(crate::js::number_to_string(v)),
        )
    }

    // ---- sources.test.mjs ----
    #[test]
    fn parse_vector_drops_nan_and_rejects_bad_shapes() {
        let body = vec_body(&[svc("a", 1.5), (json!({"service": "b"}), json!("NaN"))]);
        assert_eq!(
            parse_vector(&body),
            Ok(vec![Sample {
                labels: json!({"service": "a"}),
                value: 1.5
            }])
        );
        assert_eq!(
            parse_vector(&json!({"status": "error"})),
            Err(FetchError::source("응답 형식 오류"))
        );
    }

    #[test]
    fn merge_services_missing_5xx_series_means_zero_errors() {
        let p = |rows: &[(Value, Value)]| parse_vector(&vec_body(rows)).unwrap();
        let (requests, errors, p95) = (
            p(&[svc("a", 200.0), svc("b", 100.0)]),
            p(&[svc("b", 10.0)]),
            p(&[svc("a", 0.4)]),
        );
        let (up_total, up_count) = (p(&[svc("a", 2.0)]), p(&[svc("a", 1.0)]));
        let rows = merge_services(&Series {
            requests: &requests,
            errors: &errors,
            p95: &p95,
            up_total: &up_total,
            up_count: &up_count,
        });
        let find = |n: &str| rows.iter().find(|r| r.name() == n).unwrap();
        assert_eq!(find("a").err_ratio, Some(0.0));
        assert_eq!(find("b").err_ratio, Some(0.1));
        assert_eq!(find("a").p95, Some(0.4));
        assert_eq!(find("b").p95, None);
        assert_eq!(find("a").instances_up, 1.0);
    }

    #[test]
    fn earliest_failure_wins() {
        let e = |m: &str| Err::<Vec<Sample>, _>(FetchError::source(m));
        assert_eq!(
            first_error([(2, e("late")), (0, Ok(vec![])), (1, e("early"))]),
            Err(FetchError::source("early"))
        );
        assert_eq!(
            first_error([(0, Ok::<Vec<Sample>, FetchError>(vec![])), (1, Ok(vec![]))])
                .map(|r| r.len()),
            Ok(2)
        );
    }

    // ---- JS value edges, expected from Node ----
    #[test]
    fn values_follow_js_number() {
        let rows = [
            (json!({"service": "s"}), json!("+Inf")),
            (json!({"service": "s"}), json!("Infinity")),
            (json!({"service": "s"}), json!("")),
            (json!({"service": "s"}), json!(" 2 ")),
            (json!({"service": "s"}), json!(null)),
            (json!({}), json!(3)),
        ];
        let got: Vec<f64> = parse_vector(&vec_body(&rows))
            .unwrap()
            .iter()
            .map(|s| s.value)
            .collect();
        assert_eq!(got, vec![0.0, 2.0, 0.0, 3.0]);
        // "a5"[1] is "5"
        let odd = json!({"status": "success", "data": {"resultType": "vector", "result": [{"value": "a5"}, {"value": {"1": "7"}}, {}]}});
        assert_eq!(
            parse_vector(&odd)
                .unwrap()
                .iter()
                .map(|s| s.value)
                .collect::<Vec<_>>(),
            vec![5.0, 7.0]
        );
        let null_row =
            json!({"status": "success", "data": {"resultType": "vector", "result": [null]}});
        assert_eq!(parse_vector(&null_row), Err(FetchError::type_error(None)));
    }

    #[test]
    fn services_keep_js_map_order_and_last_value() {
        let p = |rows: &[(Value, Value)]| parse_vector(&vec_body(rows)).unwrap();
        let requests = p(&[
            svc("b", 1.0),
            (json!({}), json!("4")),
            svc("a", 2.0),
            svc("b", 5.0),
        ]);
        let p95 = p(&[svc("c", 0.1)]);
        let rows = merge_services(&Series {
            requests: &requests,
            errors: &[],
            p95: &p95,
            up_total: &[],
            up_count: &[],
        });
        let got: Vec<(String, f64)> = rows.iter().map(|r| (r.name(), r.requests)).collect();
        assert_eq!(
            got,
            vec![
                ("b".into(), 5.0),
                ("(unknown)".into(), 4.0),
                ("a".into(), 2.0),
                ("c".into(), 0.0)
            ]
        );
        assert_eq!(rows[3].err_ratio, None);
        assert_eq!(rows[3].instances, None);
        assert_eq!(
            totals_of(&rows),
            Totals {
                requests: 11.0,
                errors: 0.0,
                err_ratio: Some(0.0)
            }
        );
        assert_eq!(totals_of(&[]).err_ratio, None);
    }
}
