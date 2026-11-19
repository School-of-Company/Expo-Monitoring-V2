// JS semantics the Node version relies on implicitly: whitespace, Number(), String(), property access
// on arbitrary JSON, URL encoding. Kept in one place so every module converts values the same way.
use serde_json::Value;

/// JS WhiteSpace + LineTerminator (what `String.prototype.trim` and `Number()` strip).
/// Differs from `char::is_whitespace`: JS includes U+FEFF, excludes U+0085.
pub fn is_js_space(c: char) -> bool {
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

/// JS `Number(value)` for a JSON value (`None` = undefined).
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_number(s),
        Some(a @ Value::Array(_)) => js_number(&to_js_string(Some(a))),
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// JS `String(value)` for a JSON value (`None` = undefined).
pub fn to_js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        // Array.prototype.join: null/undefined elements become ""
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                if x.is_null() {
                    String::new()
                } else {
                    to_js_string(Some(x))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// `a ?? b`: JSON null and a missing value are both nullish.
pub fn nullish(v: Option<&Value>) -> Option<&Value> {
    v.filter(|x| !x.is_null())
}

/// JS truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `x?.[key]` where `x` may be any JSON value: arrays and strings index by position, objects by key.
/// Returns None for undefined. Reading a property of null/undefined itself is the caller's TypeError.
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(key),
        Value::Array(a) => key
            .parse::<usize>()
            .ok()
            .filter(|i| i.to_string() == key)
            .and_then(|i| a.get(i)),
        _ => None,
    }
}

/// `x[i]` for an integer index: array element, string code unit (as a 1-unit string), object key "i".
pub fn index(v: &Value, i: usize) -> Option<Value> {
    match v {
        Value::Array(a) => a.get(i).cloned(),
        Value::String(s) => s
            .encode_utf16()
            .nth(i)
            .map(|u| Value::String(String::from_utf16_lossy(&[u]))),
        Value::Object(m) => m.get(&i.to_string()).cloned(),
        _ => None,
    }
}

/// `Map` key equality (SameValueZero): numbers by value, objects/arrays never equal (identity).
pub fn same_key(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => false,
        _ => a == b,
    }
}

/// An insertion-ordered `Map` with JS key equality: `set` keeps the first position and the last value.
#[derive(Debug, Clone)]
pub struct JsMap<V> {
    pub entries: Vec<(Value, V)>,
}

impl<V> Default for JsMap<V> {
    fn default() -> Self {
        JsMap {
            entries: Vec::new(),
        }
    }
}

impl<V> JsMap<V> {
    pub fn position(&self, k: &Value) -> Option<usize> {
        self.entries.iter().position(|(key, _)| same_key(key, k))
    }
    pub fn get(&self, k: &Value) -> Option<&V> {
        self.position(k).map(|i| &self.entries[i].1)
    }
    pub fn set(&mut self, k: Value, v: V) {
        match self.position(&k) {
            Some(i) => self.entries[i].1 = v,
            None => self.entries.push((k, v)),
        }
    }
}

/// JS `Number.prototype.toString()` (shortest round-trip digits, exponent past 1e21 / below 1e-6).
pub fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // Rust's `{:e}` prints the shortest round-trip digits: "d.ddddde-7"
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let mut digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    if let Some(even) = tie_to_even(x.abs(), &digits, exp) {
        digits = even;
    }
    let k = digits.len() as i32;
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e = if e >= 0 {
            format!("+{e}")
        } else {
            e.to_string()
        };
        if k == 1 {
            format!("{digits}e{e}")
        } else {
            format!("{}.{}e{e}", &digits[..1], &digits[1..])
        }
    };
    format!("{sign}{body}")
}

/// When `x` lies exactly halfway between two shortest candidates, JS picks the even last digit; Rust rounds up.
fn tie_to_even(x: f64, digits: &str, exp: i32) -> Option<String> {
    let last = digits.bytes().last()?;
    if (last - b'0').is_multiple_of(2) || digits.len() < 2 {
        return None;
    }
    // exact decimal expansion of x (every f64 has at most ~767 significant digits)
    let exact = format!("{:.800e}", x);
    let (m, e) = exact.split_once('e')?;
    let exact_digits: String = m.chars().filter(char::is_ascii_digit).collect();
    let exact_digits = exact_digits.trim_end_matches('0');
    let lower = format!("{}{}", &digits[..digits.len() - 1], char::from(last - 1));
    let is_tie = e.parse::<i32>().ok()? == exp
        && exact_digits.len() == digits.len() + 1
        && exact_digits.ends_with('5')
        && exact_digits.starts_with(&lower);
    let round_trips = format!("{}.{}e{exp}", &lower[..1], &lower[1..])
        .parse::<f64>()
        .ok()?
        == x;
    (is_tie && round_trips).then_some(lower)
}

/// End index of `arr.slice(0, n)` (ToIntegerOrInfinity, negative counts from the end).
pub fn slice_end(len: usize, n: f64) -> usize {
    if n.is_nan() {
        return 0;
    }
    let n = n.trunc();
    if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        n.min(len as f64) as usize
    }
}

fn percent_encode(s: &str, keep: impl Fn(u8) -> bool, space_plus: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' if space_plus => out.push('+'),
            b if keep(b) => out.push(char::from(b)),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// JS `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    percent_encode(
        s,
        |b| b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b),
        false,
    )
}

/// JS `new URLSearchParams(pairs).toString()` (application/x-www-form-urlencoded, space as `+`).
pub fn search_params(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        percent_encode(
            s,
            |b| b.is_ascii_alphanumeric() || b"*-._".contains(&b),
            true,
        )
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_whitespace_rules() {
        assert_eq!(js_number("\u{FEFF}4"), 4.0);
        assert!(js_number("\u{85}4").is_nan());
    }

    // expected values from Node
    #[test]
    fn number_to_string_like_js() {
        for (x, s) in [
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.7612e18, "1761200000000000000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (-0.0, "0"),
            (-2055859200000000.2, "-2055859200000000.2"),
            (-2066953604251.4062, "-2066953604251.4062"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(number_to_string(x), s, "{x:e}");
        }
    }

    #[test]
    fn to_number_and_string_of_json() {
        assert_eq!(to_number(Some(&json!("1.5"))), 1.5);
        assert_eq!(to_number(Some(&json!(""))), 0.0);
        assert_eq!(to_number(Some(&json!(null))), 0.0);
        assert_eq!(to_number(Some(&json!(true))), 1.0);
        assert_eq!(to_number(Some(&json!(["7"]))), 7.0);
        assert_eq!(to_number(Some(&json!([]))), 0.0);
        assert!(to_number(Some(&json!([1, 2]))).is_nan());
        assert!(to_number(Some(&json!("NaN"))).is_nan());
        assert!(to_number(None).is_nan());
        assert_eq!(
            to_js_string(Some(&json!([1, null, "a", [2, 3]]))),
            "1,,a,2,3"
        );
        assert_eq!(to_js_string(Some(&json!({"a": 1}))), "[object Object]");
        assert_eq!(to_js_string(Some(&json!(1e21))), "1e+21");
        assert_eq!(to_js_string(None), "undefined");
    }

    #[test]
    fn property_access() {
        assert_eq!(get(&json!([1, "2"]), "1"), Some(&json!("2")));
        assert_eq!(get(&json!({"1": 9}), "1"), Some(&json!(9)));
        assert_eq!(get(&json!([1, 2]), "01"), None);
        assert_eq!(get(&json!("ab"), "service"), None);
    }

    #[test]
    fn slice_end_like_js() {
        assert_eq!(slice_end(10, 5.0), 5);
        assert_eq!(slice_end(3, 5.0), 3);
        assert_eq!(slice_end(10, 2.9), 2);
        assert_eq!(slice_end(10, -1.0), 9);
        assert_eq!(slice_end(2, -5.0), 0);
        assert_eq!(slice_end(4, f64::INFINITY), 4);
        assert_eq!(slice_end(4, f64::NAN), 0);
    }

    #[test]
    fn url_encoding_like_js() {
        assert_eq!(
            encode_uri_component("msa overview/ü!*'()~"),
            "msa%20overview%2F%C3%BC!*'()~"
        );
        assert_eq!(
            search_params(&[
                ("query", "sum by (service) (x{a=\"b\"}[86400s])"),
                ("time", "1.500")
            ]),
            "query=sum+by+%28service%29+%28x%7Ba%3D%22b%22%7D%5B86400s%5D%29&time=1.500"
        );
        assert_eq!(search_params(&[("q", "~!'*-._ü")]), "q=%7E%21%27*-._%C3%BC");
    }
}
