// Port of digest/loki.mjs (parsing part; the HTTP collection comes with the report step).
use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::http::FetchError;
use crate::js::{get, index, is_js_space, js_trim, nullish, slice_end, to_js_string};
use crate::mask::{JS_SPACE, mask, truncate};

/// Message used for grouping: JSON "message"/"msg"/"error"/"err" field if it is a string, else the first line.
// shortcut: serde_json rejects lone-surrogate escapes that JSON.parse accepts; such a line keeps its raw first line.
pub fn message_of(line: Option<&Value>) -> String {
    let mut text = to_js_string(line);
    if text.trim_start_matches(is_js_space).starts_with('{')
        && let Ok(o) = serde_json::from_str::<Value>(&text)
    {
        let m = ["message", "msg", "error", "err"]
            .iter()
            .find_map(|k| nullish(get(&o, k)));
        if let Some(Value::String(m)) = m {
            text = m.clone();
        }
    }
    match text.split_once('\n') {
        Some((first, _)) => first.to_string(),
        None => text,
    }
}

static DIGITS: LazyLock<Result<regex::Regex, regex::Error>> =
    LazyLock::new(|| regex::Regex::new("[0-9]{2,}"));
static SPACES: LazyLock<Result<regex::Regex, regex::Error>> =
    LazyLock::new(|| regex::Regex::new(&format!("[{JS_SPACE}]+")));

/// Group key: masked, volatile numbers collapsed, cut to 80 UTF-16 units.
pub fn group_key(line: Option<&Value>) -> String {
    let (Ok(digits), Ok(spaces)) = (&*DIGITS, &*SPACES) else {
        return "***".to_string();
    };
    let masked = mask(&message_of(line));
    let collapsed = digits.replace_all(&masked, "#");
    let spaced = spaces.replace_all(&collapsed, " ");
    truncate(js_trim(&spaced), 80)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TopError {
    pub message: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Groups {
    pub total: u64,
    pub kinds: usize,
    pub top: Vec<TopError>,
}

/// `s.values ?? []` as an array; anything else would make the JS loop throw.
// shortcut: a string `values` is iterated char by char in JS; here it is a TypeError like other non-arrays.
fn values_of(s: &Value) -> Result<&[Value], FetchError> {
    if s.is_null() {
        return Err(FetchError::type_error(None));
    }
    match nullish(get(s, "values")) {
        None => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(FetchError::type_error(None)),
    }
}

pub fn group_errors(streams: &Value, top_n: f64) -> Result<Groups, FetchError> {
    let streams = streams
        .as_array()
        .ok_or_else(|| FetchError::type_error(None))?;
    let mut counts: Vec<(String, u64)> = Vec::new();
    let mut pos: HashMap<String, usize> = HashMap::new();
    let mut total = 0;
    for s in streams {
        for entry in values_of(s)? {
            if entry.is_null() {
                return Err(FetchError::type_error(None)); // entry[1] on null
            }
            let key = group_key(index(entry, 1).as_ref());
            if key.is_empty() {
                continue;
            }
            match pos.get(&key) {
                Some(&i) => counts[i].1 += 1,
                None => {
                    pos.insert(key.clone(), counts.len());
                    counts.push((key, 1));
                }
            }
            total += 1;
        }
    }
    let kinds = counts.len();
    counts.sort_by_key(|c| std::cmp::Reverse(c.1)); // stable, like Array.prototype.sort
    counts.truncate(slice_end(kinds, top_n));
    Ok(Groups {
        total,
        kinds,
        top: counts
            .into_iter()
            .map(|(message, count)| TopError { message, count })
            .collect(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct LokiSummary {
    pub groups: Groups,
    /// The line limit was reached, so counts are a sample.
    pub sampled: bool,
}

/// `query_range` body -> grouped errors (the part of `collectLoki` after the fetch).
pub fn parse_streams(body: &Value, log_top_n: f64, limit: f64) -> Result<LokiSummary, FetchError> {
    let data = get(body, "data");
    if get(body, "status") != Some(&json!("success"))
        || data.and_then(|d| get(d, "resultType")) != Some(&json!("streams"))
    {
        return Err(FetchError::source("응답 형식 오류"));
    }
    let result = data.and_then(|d| get(d, "result")).unwrap_or(&Value::Null);
    let groups = group_errors(result, log_top_n)?;
    let lines = result.as_array().map_or(0, |a| {
        a.iter()
            .filter_map(|s| values_of(s).ok())
            .map(<[Value]>::len)
            .sum::<usize>()
    });
    Ok(LokiSummary {
        groups,
        sampled: lines as f64 >= limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- sources.test.mjs ----
    #[test]
    fn grouping_masks_pii_and_uses_only_the_message_field() {
        let streams = json!([{"stream": {}, "values": [
            ["1", json!({"message": "send failed to 010-1234-5678", "body": {"phone": "01099998888"}}).to_string()],
            ["2", json!({"message": "send failed to 010-9999-0000"}).to_string()],
            ["3", "user a@b.co not found\nstack trace line"],
        ]}]);
        let g = group_errors(&streams, 5.0).unwrap();
        assert_eq!(g.total, 3);
        assert_eq!(
            g.top[0],
            TopError {
                message: "send failed to ***".into(),
                count: 2
            }
        );
        let all = format!("{g:?}");
        assert!(
            !all.contains("01099998888") && !all.contains("a@b.co") && !all.contains("stack trace")
        );
    }

    // ---- edges, expected values from Node ----
    #[test]
    fn message_field_rules() {
        let m = |v: Value| message_of(Some(&v));
        assert_eq!(m(json!(r#"{"msg":"a","message":null}"#)), "a");
        assert_eq!(
            m(json!(r#"{"message":5,"msg":"b"}"#)),
            r#"{"message":5,"msg":"b"}"#
        ); // 5 is not nullish, not a string
        assert_eq!(m(json!(r#"  {"err":"x\ny"}"#)), "x");
        assert_eq!(m(json!("{not json\nsecond")), "{not json");
        assert_eq!(m(json!(12)), "12");
        assert_eq!(message_of(None), "undefined");
    }

    #[test]
    fn key_collapses_numbers_and_space() {
        let k = |s: &str| group_key(Some(&json!(s)));
        assert_eq!(k("  took 1234 ms\u{3000} id 7  "), "took # ms id 7");
        assert_eq!(k(&"가".repeat(100)), format!("{}…", "가".repeat(79)));
        assert_eq!(k("   "), "");
    }

    #[test]
    fn top_n_sort_and_sampling() {
        let body = json!({"status": "success", "data": {"resultType": "streams", "result": [
            {"values": [["1", "a"], ["2", "b"], ["3", "b"], ["4", "c"], ["5", " "]]},
            {"values": null},
            {"values": [["6", "c"], ["7", "a"]]},
        ]}});
        let s = parse_streams(&body, 2.0, 7.0).unwrap();
        assert_eq!(s.groups.total, 6);
        assert_eq!(s.groups.kinds, 3);
        // ties keep first-seen order
        assert_eq!(
            s.groups
                .top
                .iter()
                .map(|t| t.message.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(s.sampled); // 7 lines read >= limit 7
        assert!(!parse_streams(&body, -1.0, 8.0).unwrap().sampled);
        assert_eq!(parse_streams(&body, -1.0, 8.0).unwrap().groups.top.len(), 2); // slice(0, -1)
        assert_eq!(
            parse_streams(&json!({"status": "success"}), 5.0, 1.0),
            Err(FetchError::source("응답 형식 오류"))
        );
        let not_array =
            json!({"status": "success", "data": {"resultType": "streams", "result": {}}});
        assert_eq!(
            parse_streams(&not_array, 5.0, 1.0),
            Err(FetchError::type_error(None))
        );
    }
}
