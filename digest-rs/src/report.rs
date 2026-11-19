// Port of digest/report.mjs: verdict, report fields, number/time formatting, markdown preview.
// (`collectAll` comes with the collection step.)
use chrono::DateTime;
use serde_json::Value;

use crate::config::Config;
use crate::grafana::{Deploy, Fired, RuleSummary, dashboard_link};
use crate::js::{JsMap, number_to_string, slice_end, to_js_string, truthy};
use crate::loki::LokiSummary;
use crate::mask::{JS_SPACE, mask, truncate};
use crate::prometheus::{ServiceRow, Totals};
use crate::schedule::Zone;

/// One source's outcome (JS `settle`: `{value}` | `{error}` | `{skipped: true}`).
#[derive(Debug, Clone, PartialEq)]
pub enum Collected<T> {
    Ok(T),
    Failed(String),
    Skipped,
}

impl<T> Collected<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Collected::Ok(v) => Some(v),
            _ => None,
        }
    }
    /// `data.x.error` when truthy.
    fn error(&self) -> Option<&str> {
        match self {
            Collected::Failed(e) if !e.is_empty() => Some(e),
            _ => None,
        }
    }
    /// `${data.x.error}` in a template (undefined when not failed).
    fn error_text(&self) -> String {
        match self {
            Collected::Failed(e) => e.clone(),
            _ => "undefined".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Daily,
    Weekly,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Daily => "daily",
            Kind::Weekly => "weekly",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRow {
    pub status: Value,
    pub count: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromAlert {
    pub name: Value,
    pub severity: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromData {
    pub services: Vec<ServiceRow>,
    pub totals: Totals,
    pub outbox: Vec<OutboxRow>,
    pub outbox_age: Option<f64>,
    pub pool_waiting: Option<f64>,
    pub alerts: Vec<PromAlert>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertsData {
    pub rules: RuleSummary,
    pub fired: Vec<Fired>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub panel_id: f64,
    pub name: String,
    pub buffer: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Images {
    pub images: Vec<Image>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Data {
    pub kind: Kind,
    pub now: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    pub prom: Collected<PromData>,
    pub prev: Collected<PromData>,
    pub alerts: Collected<AlertsData>,
    pub deploys: Collected<Vec<Deploy>>,
    pub snapshot: Collected<Snapshot>,
    pub images: Collected<Images>,
    pub loki: Collected<LokiSummary>,
    pub dry_run: bool,
}

// ---- formatting -----------------------------------------------------------

/// JS `Math.round`: halves go toward +Infinity, and -0 survives.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() || x.fract() == 0.0 {
        return x;
    }
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    if r == 0.0 && x < 0.0 { -0.0 } else { r }
}

/// Insert en-US thousands separators into an unsigned digit string.
fn group_thousands(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `Intl.NumberFormat('en-US').format(n)` for an integer-valued n (ICU prints the shortest digits, zero-padded).
fn intl_int(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    let sign = if n.is_sign_negative() { "-" } else { "" };
    if n.is_infinite() {
        return format!("{sign}∞");
    }
    let s = number_to_string(n.abs());
    let digits = match s.split_once("e+") {
        Some((m, e)) => {
            let m = m.replace('.', "");
            let zeros = e.parse::<usize>().unwrap_or(0) + 1 - m.len();
            format!("{m}{}", "0".repeat(zeros))
        }
        None => s,
    };
    format!("{sign}{}", group_thousands(&digits))
}

/// `int(n)` = `nf.format(Math.round(n))`.
pub fn int(n: f64) -> String {
    intl_int(js_round(n))
}

/// JS `Number.prototype.toFixed(d)`: rounds the exact binary value, exact halves away from zero.
pub fn to_fixed(x: f64, d: usize) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.abs() >= 1e21 {
        return number_to_string(x);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // exact decimal expansion (an f64 has at most 1074 fractional digits)
    let exact = format!("{:.1100}", x.abs());
    let (int_part, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut digits: Vec<u8> = int_part.bytes().chain(frac.bytes().take(d)).collect();
    if frac.as_bytes().get(d).is_some_and(|b| *b >= b'5') {
        // round up the decimal digit string
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let int_len = digits.len() - d;
    let s = String::from_utf8_lossy(&digits);
    if d == 0 {
        format!("{sign}{s}")
    } else {
        format!("{sign}{}.{}", &s[..int_len], &s[int_len..])
    }
}

pub fn pct(r: f64) -> String {
    format!("{}%", to_fixed(r * 100.0, 2))
}

pub fn secs(s: f64) -> String {
    if s < 1.0 {
        format!("{}ms", number_to_string(js_round(s * 1000.0)))
    } else {
        format!("{}s", to_fixed(s, 2))
    }
}

fn sign(n: f64) -> &'static str {
    if n >= 0.0 { "+" } else { "-" }
}

fn d_pct(a: f64, b: f64) -> String {
    format!("{}{}%p", sign(a - b), to_fixed((a - b).abs() * 100.0, 2))
}

fn d_rel(a: f64, b: f64) -> String {
    if b > 0.0 {
        format!("{}{}%", sign(a - b), to_fixed((a - b).abs() / b * 100.0, 0))
    } else {
        "n/a".to_string()
    }
}

/// `new Date(value)` time value for a JSON value; None = Invalid Date.
// shortcut: strings are parsed as RFC 3339 only; Date.parse also accepts other formats.
pub fn date_value(v: Option<&Value>) -> Option<i64> {
    let ms = match v? {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => {
            return DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp_millis());
        }
        _ => return None,
    };
    (ms.is_finite() && ms.abs() <= 8.64e15).then(|| ms.trunc() as i64)
}

/// `fmtTime`: `YYYY-MM-DD HH:MM` in `tz`. Errors carry the JS RangeError message.
pub fn fmt_time(ms: Option<i64>, tz: &str) -> Result<String, String> {
    let zone = Zone::parse(tz)?;
    let w = ms
        .and_then(|ms| zone.wall(ms))
        .ok_or_else(|| "Invalid time value".to_string())?;
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        w.year, w.month, w.day, w.hour, w.minute
    ))
}

fn failure(what: &str, why: &str) -> String {
    format!("{what}: 수집 실패({why})")
}

/// Stable sort with a JS comparator; NaN counts as equal.
fn js_sort<T>(v: &mut [T], cmp: impl Fn(&T, &T) -> f64) {
    v.sort_by(|a, b| {
        cmp(a, b)
            .partial_cmp(&0.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

// ---- verdict ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// 0 ok, 1 warn, 2 crit
    pub level: usize,
    pub label: &'static str,
    pub reasons: Vec<String>,
}

fn is_critical(v: &Value) -> bool {
    v.as_str() == Some("critical")
}

pub fn judge(data: &Data, cfg: &Config) -> Verdict {
    let t = &cfg.thresholds;
    let mut reasons = Vec::new();
    let mut level = 0;
    let mut flag = |lv: usize, text: String| {
        level = level.max(lv);
        reasons.push(text);
    };
    let p = data.prom.value();
    if let Some(p) = p {
        let eligible: Vec<&ServiceRow> = p
            .services
            .iter()
            .filter(|s| s.requests >= cfg.min_requests)
            .collect();
        let mut by_err = eligible.clone();
        js_sort(&mut by_err, |a, b| {
            b.err_ratio.unwrap_or(0.0) - a.err_ratio.unwrap_or(0.0)
        });
        let worst = by_err.first();
        let overall = if p.totals.requests >= cfg.min_requests {
            p.totals.err_ratio
        } else {
            None
        };
        let top = js_max(
            worst.and_then(|w| w.err_ratio).unwrap_or(0.0),
            overall.unwrap_or(0.0),
        );
        let who = match worst {
            Some(w) if w.err_ratio.unwrap_or(0.0) >= overall.unwrap_or(0.0) => w.name(),
            _ => "전체".to_string(),
        };
        if top * 100.0 > t.err5xx_crit_pct {
            flag(2, format!("5xx 비율 {} ({who})", pct(top)));
        } else if top * 100.0 > t.err5xx_warn_pct {
            flag(1, format!("5xx 비율 {} ({who})", pct(top)));
        }
        let mut slow: Vec<&ServiceRow> = eligible
            .iter()
            .copied()
            .filter(|s| s.p95.is_some())
            .collect();
        js_sort(&mut slow, |a, b| {
            b.p95.unwrap_or(0.0) - a.p95.unwrap_or(0.0)
        });
        if let Some(s) = slow.first()
            && let Some(p95) = s.p95
        {
            if p95 > t.p95_crit_sec {
                flag(2, format!("p95 {} ({})", secs(p95), s.name()));
            } else if p95 > t.p95_warn_sec {
                flag(1, format!("p95 {} ({})", secs(p95), s.name()));
            }
        }
        let down: Vec<String> = p
            .services
            .iter()
            .filter(|s| s.instances.is_some_and(|i| s.instances_up < i))
            .map(ServiceRow::name)
            .collect();
        if !down.is_empty() {
            flag(1, format!("인스턴스 down: {}", down.join(", ")));
        }
        if let Some(age) = p.outbox_age {
            if age > t.outbox_age_crit_sec {
                flag(2, format!("Form outbox 대기 {}", secs(age)));
            } else if age > t.outbox_age_warn_sec {
                flag(1, format!("Form outbox 대기 {}", secs(age)));
            }
        }
        if let Some(w) = p.pool_waiting
            && w >= t.pool_waiting_warn
        {
            flag(1, format!("Form DB 풀 대기 {}", int(w)));
        }
        if p.alerts.iter().any(|a| is_critical(&a.severity)) {
            flag(2, "critical 알림 발화(Prometheus)".to_string());
        } else if !p.alerts.is_empty() {
            flag(1, format!("알림 발화 {}건(Prometheus)", p.alerts.len()));
        }
    }
    if let Some(g) = data.alerts.value() {
        if g.rules.firing.iter().any(|a| is_critical(&a.severity)) {
            flag(2, "critical 알림 발화(Grafana)".to_string());
        } else if !g.rules.firing.is_empty() {
            flag(1, format!("알림 발화 {}건(Grafana)", g.rules.firing.len()));
        }
        if g.rules.counts.error > 0 {
            flag(1, format!("알림 규칙 오류 {}건", g.rules.counts.error));
        }
    }
    let unknown = level == 0 && p.is_none();
    let label = if unknown {
        "판정 불가"
    } else {
        ["이상 없음", "주의", "위험"][level]
    };
    Verdict {
        level,
        label,
        reasons,
    }
}

/// `Math.max(a, b)`: NaN wins.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

// ---- report ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub verdict: Verdict,
    pub title: String,
    pub description: String,
    pub color: u32,
    pub footer: String,
    pub fields: Vec<Field>,
    pub images: Vec<Image>,
}

const COLORS: [u32; 3] = [0x2ecc71, 0xf39c12, 0xe74c3c];

type PrevFmt<'a> = Option<&'a dyn Fn(&ServiceRow, &ServiceRow) -> String>;

fn top_lines(
    services: &[&ServiceRow],
    prev: Option<&JsMap<&ServiceRow>>,
    n: f64,
    fmt: impl Fn(&ServiceRow) -> String,
    prev_fmt: PrevFmt,
) -> String {
    services[..slice_end(services.len(), n)]
        .iter()
        .map(|s| {
            let extra = match (prev.and_then(|m| m.get(&s.service)), prev_fmt) {
                (Some(o), Some(f)) => f(s, o),
                _ => String::new(),
            };
            format!("{}  {}{extra}", s.name(), fmt(s))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `mask(a.name) + (a.severity ? ` (${a.severity})` : '')`
fn alert_label(name: &str, severity: &Value) -> String {
    let sev = if truthy(Some(severity)) {
        format!(" ({})", to_js_string(Some(severity)))
    } else {
        String::new()
    };
    format!("{}{sev}", mask(name))
}

fn collapse_space(s: &str) -> String {
    match regex::Regex::new(&format!("[{JS_SPACE}]+")) {
        Ok(re) => re.replace_all(s, " ").into_owned(),
        Err(_) => s.to_string(),
    }
}

/// `buildReport`. Fails only where JS throws (invalid time zone, deploy with an invalid time).
pub fn build_report(data: &Data, cfg: &Config) -> Result<Report, String> {
    let weekly = data.kind == Kind::Weekly;
    let v = judge(data, cfg);
    let p = data.prom.value();
    let prev = data.prev.value();
    let prev_by_name = prev.map(|prev| {
        let mut m = JsMap::default();
        for s in &prev.services {
            m.set(s.service.clone(), s);
        }
        m
    });
    let mut fields = Vec::new();
    let mut field = |name: String, value: String| fields.push(Field { name, value });
    let title = if weekly {
        "Expo 주간 요약 - 지난주 vs 이번주"
    } else {
        "Expo 일일 요약 - 어제 한눈에"
    };
    let period = format!(
        "{} ~ {} ({})",
        fmt_time(Some(data.from_ms), &cfg.tz)?,
        fmt_time(Some(data.to_ms), &cfg.tz)?,
        cfg.tz
    );

    // description: verdict + key numbers
    let reasons = if v.reasons.is_empty() {
        String::new()
    } else {
        format!(" - {}", v.reasons[..v.reasons.len().min(4)].join(" / "))
    };
    let mut lines = vec![format!("**판정: {}**{reasons}", v.label)];
    if let Some(p) = p {
        let mut slowest: Vec<&ServiceRow> = p.services.iter().filter(|s| s.p95.is_some()).collect();
        js_sort(&mut slowest, |a, b| {
            b.p95.unwrap_or(0.0) - a.p95.unwrap_or(0.0)
        });
        let up = p.services.iter().fold(0.0, |a, s| a + s.instances_up);
        let all = p
            .services
            .iter()
            .fold(0.0, |a, s| a + s.instances.unwrap_or(0.0));
        let ratio = p.totals.err_ratio.map_or_else(|| "-".to_string(), pct);
        let mut l = format!(
            "요청 {}건 · 5xx {ratio} ({}건)",
            int(p.totals.requests),
            int(p.totals.errors)
        );
        if let Some(s) = slowest.first() {
            l += &format!(" · 최대 p95 {} ({})", secs(s.p95.unwrap_or(0.0)), s.name());
        }
        if all != 0.0 && !all.is_nan() {
            l += &format!(
                " · 인스턴스 {}/{} up",
                number_to_string(up),
                number_to_string(all)
            );
        }
        lines.push(l);
        match prev {
            Some(prev) if weekly && prev.totals.requests > 0.0 => {
                let mut c = format!(
                    "지난주 대비: 요청 {}",
                    d_rel(p.totals.requests, prev.totals.requests)
                );
                if let (Some(a), Some(b)) = (p.totals.err_ratio, prev.totals.err_ratio) {
                    c += &format!(" · 5xx {}", d_pct(a, b));
                }
                lines.push(c);
            }
            _ if weekly => lines.push("지난주 대비: 비교 데이터 없음".to_string()),
            _ => {}
        }
    } else {
        lines.push(failure("Prometheus 수치", &data.prom.error_text()));
    }
    lines.push(period);

    // 1. alerts
    let mut alert_lines = Vec::new();
    if let Some(g) = data.alerts.value() {
        let c = g.rules.counts;
        alert_lines.push(format!(
            "Grafana 규칙: 정상 {} · 대기 {} · 발화 {} · 오류 {}",
            c.normal, c.pending, c.firing, c.error
        ));
        if !g.rules.firing.is_empty() {
            let names: Vec<String> = g
                .rules
                .firing
                .iter()
                .map(|a| alert_label(&a.name, &a.severity))
                .collect();
            alert_lines.push(format!("발화 중: {}", names.join(", ")));
        }
        if !g.fired.is_empty() {
            let names: Vec<String> = g
                .fired
                .iter()
                .map(|a| format!("{} x{}", mask(&to_js_string(Some(&a.name))), a.times))
                .collect();
            alert_lines.push(format!("기간 내 발화: {}", names.join(", ")));
        }
    } else if let Some(e) = data.alerts.error() {
        alert_lines.push(failure("Grafana 알림", e));
    }
    if let Some(p) = p.filter(|p| !p.alerts.is_empty()) {
        let names: Vec<String> = p
            .alerts
            .iter()
            .map(|a| alert_label(&to_js_string(Some(&a.name)), &a.severity))
            .collect();
        alert_lines.push(format!("Prometheus 알림 발화: {}", names.join(", ")));
    }
    if !alert_lines.is_empty() {
        field("알림".to_string(), alert_lines.join("\n"));
    }

    // 2-3. top errors / latency
    let top_n = number_to_string(cfg.top_n);
    if let Some(p) = p {
        let mut err_top: Vec<&ServiceRow> = p
            .services
            .iter()
            .filter(|s| s.requests > 0.0 && s.err_ratio.is_some_and(|r| r > 0.0))
            .collect();
        js_sort(&mut err_top, |a, b| {
            b.err_ratio.unwrap_or(0.0) - a.err_ratio.unwrap_or(0.0)
        });
        let prev_err = |s: &ServiceRow, o: &ServiceRow| match (o.err_ratio, s.err_ratio) {
            (Some(o), Some(s)) => format!(" [지난주 {}, {}]", pct(o), d_pct(s, o)),
            _ => String::new(),
        };
        let value = if err_top.is_empty() {
            "5xx 없음".to_string()
        } else {
            let fmt = |s: &ServiceRow| {
                format!(
                    "{} ({}/{}건)",
                    pct(s.err_ratio.unwrap_or(0.0)),
                    int(s.errors),
                    int(s.requests)
                )
            };
            top_lines(
                &err_top,
                prev_by_name.as_ref(),
                cfg.top_n,
                fmt,
                if weekly { Some(&prev_err) } else { None },
            )
        };
        field(format!("5xx 비율 상위 {top_n}"), value);

        let mut slow: Vec<&ServiceRow> = p.services.iter().filter(|s| s.p95.is_some()).collect();
        js_sort(&mut slow, |a, b| {
            b.p95.unwrap_or(0.0) - a.p95.unwrap_or(0.0)
        });
        let prev_p95 = |_: &ServiceRow, o: &ServiceRow| {
            o.p95
                .map_or_else(String::new, |p| format!(" [지난주 {}]", secs(p)))
        };
        let value = if slow.is_empty() {
            "지연 데이터 없음 (히스토그램 필요)".to_string()
        } else {
            top_lines(
                &slow,
                prev_by_name.as_ref(),
                cfg.top_n,
                |s| secs(s.p95.unwrap_or(0.0)),
                if weekly { Some(&prev_p95) } else { None },
            )
        };
        field(format!("p95 지연 상위 {top_n}"), value);
    } else {
        field(
            "Prometheus".to_string(),
            failure("요청·오류·지연·인스턴스", &data.prom.error_text()),
        );
    }

    // 4. logs
    if let Some(l) = data.loki.value() {
        let g = &l.groups;
        let value = if g.top.is_empty() {
            "오류 로그 없음".to_string()
        } else {
            let top: Vec<String> = g
                .top
                .iter()
                .map(|t| format!("{}x {}", int(t.count as f64), t.message))
                .collect();
            let sampled = if l.sampled {
                ", 표본 상한 도달"
            } else {
                ""
            };
            format!(
                "{}\n(총 {}건{sampled}, 개인정보 마스킹됨)",
                top.join("\n"),
                int(g.total as f64)
            )
        };
        field(
            format!("오류 로그 유형 상위 {}", number_to_string(cfg.log_top_n)),
            value,
        );
    } else if let Some(e) = data.loki.error() {
        field("오류 로그".to_string(), failure("Loki", e));
    }

    // 5. instances / form
    if let Some(p) = p {
        let mut out = Vec::new();
        let down: Vec<String> = p
            .services
            .iter()
            .filter_map(|s| {
                s.instances.filter(|i| s.instances_up < *i).map(|i| {
                    format!(
                        "{} {}/{}",
                        s.name(),
                        number_to_string(s.instances_up),
                        number_to_string(i)
                    )
                })
            })
            .collect();
        out.push(if down.is_empty() {
            "모든 인스턴스 up".to_string()
        } else {
            format!("down: {}", down.join(", "))
        });
        if !p.outbox.is_empty() {
            let rows: Vec<String> = p
                .outbox
                .iter()
                .map(|o| {
                    if truthy(Some(&o.status)) {
                        format!("{} {}", to_js_string(Some(&o.status)), int(o.count))
                    } else {
                        int(o.count)
                    }
                })
                .collect();
            out.push(format!("Form outbox: {}", rows.join(", ")));
        }
        if let Some(age) = p.outbox_age {
            out.push(format!("Form outbox 최장 대기: {}", secs(age)));
        }
        if let Some(w) = p.pool_waiting {
            out.push(format!("Form DB 풀 대기 최대: {}", int(w)));
        }
        field("인스턴스·Form".to_string(), out.join("\n"));
    }

    // 6. deploys
    if let Some(d) = data.deploys.value() {
        let value = if d.is_empty() {
            "기간 내 배포 기록 없음".to_string()
        } else {
            let rows = d[..d.len().min(8)]
                .iter()
                .map(|x| {
                    Ok(format!(
                        "{}  {}",
                        fmt_time(date_value(x.time.as_ref()), &cfg.tz)?,
                        truncate(&collapse_space(&mask(&x.text)), 100)
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?;
            rows.join("\n")
        };
        field("최근 배포".to_string(), value);
    } else if let Some(e) = data.deploys.error() {
        field("최근 배포".to_string(), failure("Grafana 주석", e));
    }

    // 7. links
    if !cfg.grafana_url.is_empty() {
        let link = dashboard_link(cfg, data.from_ms, data.to_ms);
        let mut out = Vec::new();
        if let Some(s) = data.snapshot.value() {
            out.push(format!("스냅샷: {}", s.url));
        } else if let Some(e) = data.snapshot.error() {
            out.push(format!("{} - 대시보드: {link}", failure("스냅샷", e)));
        } else {
            out.push(format!(
                "대시보드: {link}{}",
                if data.dry_run {
                    " (dry-run: 스냅샷 생성 생략)"
                } else {
                    ""
                }
            ));
        }
        if let Some(e) = data.images.error() {
            out.push(failure("패널 이미지", e));
        } else if let Some(i) = data.images.value().filter(|i| !i.failures.is_empty()) {
            out.push(format!("패널 이미지 일부 실패: {}", i.failures.join("; ")));
        }
        field("링크".to_string(), out.join("\n"));
    }

    Ok(Report {
        title: title.to_string(),
        description: lines.join("\n"),
        color: COLORS[v.level],
        footer: format!("{} digest", data.kind.as_str()),
        fields,
        images: data
            .images
            .value()
            .map(|i| i.images.clone())
            .unwrap_or_default(),
        verdict: v,
    })
}

pub fn to_markdown(r: &Report) -> String {
    let mut out = vec![
        format!("# {}", r.title),
        String::new(),
        r.description.clone(),
        String::new(),
    ];
    for f in &r.fields {
        out.extend([format!("## {}", f.name), f.value.clone(), String::new()]);
    }
    if !r.images.is_empty() {
        let list: Vec<String> = r
            .images
            .iter()
            .map(|i| format!("{} ({} bytes)", i.name, i.buffer.len()))
            .collect();
        out.extend(["## 첨부 이미지".to_string(), list.join("\n"), String::new()]);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use crate::grafana::FiringRule;
    use crate::grafana::RuleCounts;
    use serde_json::json;

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

    fn data(prom: Collected<PromData>, alerts: Collected<AlertsData>) -> Data {
        Data {
            kind: Kind::Daily,
            now: 0,
            from_ms: 0,
            to_ms: 0,
            prom,
            prev: Collected::Skipped,
            alerts,
            deploys: Collected::Skipped,
            snapshot: Collected::Skipped,
            images: Collected::Skipped,
            loki: Collected::Skipped,
            dry_run: false,
        }
    }

    fn one_service(ratio: f64) -> Collected<PromData> {
        let s = ServiceRow {
            service: json!("s"),
            requests: 1000.0,
            errors: ratio * 1000.0,
            err_ratio: Some(ratio),
            p95: Some(0.1),
            instances: Some(1.0),
            instances_up: 1.0,
        };
        Collected::Ok(PromData {
            services: vec![s],
            totals: Totals {
                requests: 1000.0,
                errors: ratio * 1000.0,
                err_ratio: Some(ratio),
            },
            outbox: vec![],
            outbox_age: None,
            pool_waiting: None,
            alerts: vec![],
        })
    }

    // ---- sources.test.mjs ----
    #[test]
    fn verdict_thresholds_are_configurable_and_strictly_greater_than() {
        let c = cfg(&[]);
        assert_eq!(
            judge(&data(one_service(0.01), Collected::Skipped), &c).label,
            "이상 없음"
        );
        assert_eq!(
            judge(&data(one_service(0.011), Collected::Skipped), &c).label,
            "주의"
        );
        assert_eq!(
            judge(&data(one_service(0.06), Collected::Skipped), &c).label,
            "위험"
        );
        assert_eq!(
            judge(
                &data(one_service(0.06), Collected::Skipped),
                &cfg(&[("DIGEST_5XX_CRIT_PCT", "10")])
            )
            .label,
            "주의"
        );
        let crit = AlertsData {
            rules: RuleSummary {
                counts: RuleCounts::default(),
                firing: vec![FiringRule {
                    name: "a".into(),
                    severity: json!("critical"),
                }],
            },
            fired: vec![],
        };
        assert_eq!(
            judge(
                &data(Collected::Failed("x".into()), Collected::Ok(crit)),
                &c
            )
            .label,
            "위험"
        );
        assert_eq!(
            judge(&data(Collected::Failed("x".into()), Collected::Skipped), &c).label,
            "판정 불가"
        );
    }

    // ---- number formatting, expected values from Node ----
    #[test]
    fn to_fixed_like_js() {
        let cases = [
            (0.125, "0.13"),
            (0.135, "0.14"),
            (1.005, "1.00"),
            (2.5, "2.50"),
            (-2.5, "-2.50"),
            (-0.001, "-0.00"),
            (-0.0, "0.00"),
            (1e21, "1e+21"),
            (1.45, "1.45"),
            (8.345, "8.35"),
            (f64::NAN, "NaN"),
            (99.999, "100.00"),
        ];
        for (x, s) in cases {
            assert_eq!(to_fixed(x, 2), s, "{x}");
        }
        assert_eq!(
            [0.5, 1.5, 2.5, -2.5].map(|x| to_fixed(x, 0)),
            ["1", "2", "3", "-3"]
        );
    }

    #[test]
    fn math_round_like_js() {
        let got = [2.5, -2.5, 0.49999999999999994, -0.4, -0.5, -0.6].map(|x| {
            let r = js_round(x);
            if r == 0.0 && r.is_sign_negative() {
                "-0".to_string()
            } else {
                number_to_string(r)
            }
        });
        assert_eq!(got, ["3", "-2", "0", "-0", "-0", "-1"]);
    }

    #[test]
    fn int_like_intl_en_us() {
        let got = [
            1234.4,
            -0.0,
            -0.4,
            1e21,
            2f64.powi(70),
            123456789012345680000.0,
            f64::NAN,
            f64::INFINITY,
            -1234567.0,
        ]
        .map(int);
        assert_eq!(
            got,
            [
                "1,234",
                "-0",
                "-0",
                "1,000,000,000,000,000,000,000",
                "1,180,591,620,717,411,300,000",
                "123,456,789,012,345,680,000",
                "NaN",
                "∞",
                "-1,234,567"
            ]
        );
    }

    #[test]
    fn small_formats() {
        assert_eq!(pct(0.0123), "1.23%");
        assert_eq!(secs(0.4567), "457ms");
        assert_eq!(secs(1.0), "1.00s");
        assert_eq!(d_pct(0.013, 0.01), "+0.30%p");
        assert_eq!(d_rel(80.0, 100.0), "-20%");
        assert_eq!(d_rel(5.0, 0.0), "n/a");
    }

    #[test]
    fn fmt_time_like_intl() {
        assert_eq!(
            fmt_time(Some(1_767_279_600_000), "Asia/Seoul"),
            Ok("2026-01-02 00:00".into())
        );
        assert_eq!(
            fmt_time(date_value(Some(&json!(null))), "Asia/Seoul"),
            Ok("1970-01-01 09:00".into())
        );
        assert_eq!(
            fmt_time(date_value(Some(&json!(-1.9))), "Asia/Seoul"),
            Ok("1970-01-01 08:59".into())
        );
        assert_eq!(
            fmt_time(
                date_value(Some(&json!("2026-01-01T00:00:00Z"))),
                "Asia/Seoul"
            ),
            Ok("2026-01-01 09:00".into())
        );
        assert_eq!(
            fmt_time(date_value(Some(&json!("1700000000000"))), "Asia/Seoul"),
            Err("Invalid time value".into())
        );
        assert_eq!(
            fmt_time(date_value(None), "Asia/Seoul"),
            Err("Invalid time value".into())
        );
        assert_eq!(
            fmt_time(Some(0), "Nope"),
            Err("Invalid time zone specified: Nope".into())
        );
    }
}
