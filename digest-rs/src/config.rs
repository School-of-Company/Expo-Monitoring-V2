// Port of digest/config.mjs. Numbers stay f64 because the Node version keeps JS numbers
// (e.g. DIGEST_TOP_N=2.5 is accepted there), and output must match it exactly.

#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    pub err5xx_warn_pct: f64,
    pub err5xx_crit_pct: f64,
    pub p95_warn_sec: f64,
    pub p95_crit_sec: f64,
    pub outbox_age_warn_sec: f64,
    pub outbox_age_crit_sec: f64,
    pub pool_waiting_warn: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub tz: String,
    pub daily_at: String,
    pub weekly_at: String,
    pub prometheus_url: String,
    pub grafana_url: String,
    pub grafana_public_url: String,
    pub grafana_token: String,
    pub grafana_user: String,
    pub grafana_password: String,
    pub loki_url: String,
    pub loki_query: String,
    pub loki_limit: f64,
    pub discord_webhook_url: String,
    pub dashboard_uid: String,
    pub panel_ids: Vec<f64>,
    pub snapshot_expires_sec: f64,
    pub top_n: f64,
    pub log_top_n: f64,
    pub min_requests: f64,
    pub thresholds: Thresholds,
}

/// JS WhiteSpace + LineTerminator (what `String.prototype.trim` and `Number()` strip).
/// Differs from `char::is_whitespace`: JS includes U+FEFF, excludes U+0085.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{D}'
            | ' '
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// JS `Number(string)`: "" and whitespace -> 0, hex/octal/binary prefixes, Infinity, else NaN.
pub fn js_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let b = t.as_bytes();
    if b.len() > 2 && b[0] == b'0' {
        let radix = match b[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if radix != 0 {
            // shortcut: f64 accumulation can be off by 1 ulp past 2^53, irrelevant for config values
            return t[2..]
                .chars()
                .try_fold(0.0_f64, |acc, c| {
                    c.to_digit(radix)
                        .map(|d| acc * f64::from(radix) + f64::from(d))
                })
                .unwrap_or(f64::NAN);
        }
    }
    if is_decimal_literal(t) {
        t.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// StrDecimalLiteral: [+-] (digits [. digits] | . digits) [e [+-] digits]. Rejects Rust-only forms like "inf"/"nan".
fn is_decimal_literal(t: &str) -> bool {
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.bytes().all(|c| c.is_ascii_digit());
    let mantissa_ok = digits(int) && digits(frac) && !(int.is_empty() && frac.is_empty());
    let exp_ok = exp.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exp_ok
}

fn num(v: Option<&str>, d: f64) -> f64 {
    match v {
        None | Some("") => d,
        Some(s) => {
            let n = js_number(s);
            if n.is_nan() { d } else { n }
        }
    }
}

fn str_or(v: Option<&str>, d: &str) -> String {
    match v {
        None | Some("") => d.to_string(),
        Some(s) => s.to_string(),
    }
}

/// `env` returns the variable's value, or None when unset.
pub fn load_config(env: impl Fn(&str) -> Option<String>) -> Config {
    let get = |k: &str| env(k);
    let s = |k: &str, d: &str| str_or(get(k).as_deref(), d);
    let n = |k: &str, d: f64| num(get(k).as_deref(), d);
    Config {
        tz: s("DIGEST_TZ", "Asia/Seoul"),
        daily_at: s("DIGEST_DAILY_AT", "09:00"),
        weekly_at: s("DIGEST_WEEKLY_AT", "MON 09:00"),
        prometheus_url: s("PROMETHEUS_URL", ""),
        grafana_url: s("GRAFANA_URL", ""),
        grafana_public_url: s("GRAFANA_PUBLIC_URL", ""),
        grafana_token: s("GRAFANA_TOKEN", ""),
        grafana_user: s("GRAFANA_USER", ""),
        grafana_password: s("GRAFANA_PASSWORD", ""),
        loki_url: s("LOKI_URL", ""),
        loki_query: s("DIGEST_LOKI_QUERY", "{level=\"ERROR\"}"),
        loki_limit: n("DIGEST_LOKI_LIMIT", 1000.0),
        discord_webhook_url: s("DISCORD_WEBHOOK_URL", ""),
        dashboard_uid: s("DIGEST_DASHBOARD_UID", "msa-overview"),
        panel_ids: s("DIGEST_PANEL_IDS", "8,10")
            .split(',')
            .map(|p| js_number(js_trim(p)))
            .filter(|n| n.is_finite() && n.fract() == 0.0 && *n > 0.0)
            .take(2)
            .collect(),
        snapshot_expires_sec: n("DIGEST_SNAPSHOT_EXPIRES_SEC", 7.0 * 24.0 * 3600.0),
        top_n: n("DIGEST_TOP_N", 5.0),
        log_top_n: n("DIGEST_LOG_TOP_N", 5.0),
        min_requests: n("DIGEST_MIN_REQUESTS", 50.0),
        thresholds: Thresholds {
            err5xx_warn_pct: n("DIGEST_5XX_WARN_PCT", 1.0),
            err5xx_crit_pct: n("DIGEST_5XX_CRIT_PCT", 5.0),
            p95_warn_sec: n("DIGEST_P95_WARN_SEC", 1.0),
            p95_crit_sec: n("DIGEST_P95_CRIT_SEC", 3.0),
            outbox_age_warn_sec: n("DIGEST_OUTBOX_AGE_WARN_SEC", 300.0),
            outbox_age_crit_sec: n("DIGEST_OUTBOX_AGE_CRIT_SEC", 1800.0),
            pool_waiting_warn: n("DIGEST_POOL_WAITING_WARN", 1.0),
        },
    }
}

/// Reads the process environment. Non-UTF-8 values are decoded lossily, like Node does.
pub fn load_config_from_env() -> Config {
    load_config(|k| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned()))
}

/// Values that must never appear in logs or error messages (length counted in UTF-16 units, like JS).
pub fn secrets_of(cfg: &Config) -> Vec<&str> {
    [
        &cfg.discord_webhook_url,
        &cfg.grafana_token,
        &cfg.grafana_password,
    ]
    .into_iter()
    .map(String::as_str)
    .filter(|s| s.encode_utf16().count() >= 4)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        load_config(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]);
        assert_eq!(c.tz, "Asia/Seoul");
        assert_eq!(c.daily_at, "09:00");
        assert_eq!(c.weekly_at, "MON 09:00");
        assert_eq!(c.loki_query, "{level=\"ERROR\"}");
        assert_eq!(c.loki_limit, 1000.0);
        assert_eq!(c.dashboard_uid, "msa-overview");
        assert_eq!(c.panel_ids, vec![8.0, 10.0]);
        assert_eq!(c.snapshot_expires_sec, 604800.0);
        assert_eq!((c.top_n, c.log_top_n, c.min_requests), (5.0, 5.0, 50.0));
        let t = &c.thresholds;
        assert_eq!(
            (
                t.err5xx_warn_pct,
                t.err5xx_crit_pct,
                t.p95_warn_sec,
                t.p95_crit_sec
            ),
            (1.0, 5.0, 1.0, 3.0)
        );
        assert_eq!(
            (
                t.outbox_age_warn_sec,
                t.outbox_age_crit_sec,
                t.pool_waiting_warn
            ),
            (300.0, 1800.0, 1.0)
        );
        assert_eq!(c.prometheus_url, "");
    }

    #[test]
    fn empty_string_means_default() {
        let c = cfg(&[("DIGEST_TZ", ""), ("DIGEST_TOP_N", "")]);
        assert_eq!(c.tz, "Asia/Seoul");
        assert_eq!(c.top_n, 5.0);
    }

    #[test]
    fn numbers_follow_js_number() {
        // from sources.test.mjs: thresholds are configurable
        assert_eq!(
            cfg(&[("DIGEST_5XX_CRIT_PCT", "10")])
                .thresholds
                .err5xx_crit_pct,
            10.0
        );
        for (raw, want) in [
            ("0x10", 16.0),
            ("1e3", 1000.0),
            (" 7 ", 7.0),
            ("2.5", 2.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("   ", 0.0),
            ("-3", -3.0),
            ("+.5e-1", 0.05),
            ("0b11", 3.0),
            ("0o17", 15.0),
            ("Infinity", f64::INFINITY),
        ] {
            assert_eq!(cfg(&[("DIGEST_TOP_N", raw)]).top_n, want, "{raw:?}");
        }
        // NaN falls back to the default
        for raw in [
            "abc", "1,5", "inf", "nan", "infinity", "0x", "1e", "+-1", ".", "1_000", "0xG", "-0x10",
        ] {
            assert_eq!(cfg(&[("DIGEST_TOP_N", raw)]).top_n, 5.0, "{raw:?}");
        }
    }

    #[test]
    fn js_whitespace_rules() {
        assert_eq!(js_number("\u{FEFF}4"), 4.0);
        assert!(js_number("\u{85}4").is_nan());
    }

    #[test]
    fn panel_ids_positive_integers_max_two() {
        assert_eq!(
            cfg(&[("DIGEST_PANEL_IDS", "3, 0, -1, 2.5, x, 8.0, 9")]).panel_ids,
            vec![3.0, 8.0]
        );
        assert_eq!(
            cfg(&[("DIGEST_PANEL_IDS", "1,2,3")]).panel_ids,
            vec![1.0, 2.0]
        );
        assert_eq!(
            cfg(&[("DIGEST_PANEL_IDS", " , ")]).panel_ids,
            Vec::<f64>::new()
        );
        assert_eq!(cfg(&[("DIGEST_PANEL_IDS", "0x8")]).panel_ids, vec![8.0]);
    }

    #[test]
    fn secrets_need_four_utf16_units() {
        let c = cfg(&[
            ("DISCORD_WEBHOOK_URL", "https://hook/x"),
            ("GRAFANA_TOKEN", "abc"),
            ("GRAFANA_PASSWORD", "😀😀"),
        ]);
        assert_eq!(secrets_of(&c), vec!["https://hook/x", "😀😀"]);
    }
}
