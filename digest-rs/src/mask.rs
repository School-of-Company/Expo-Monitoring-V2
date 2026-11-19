// Port of digest/mask.mjs. Masks personal data before anything from logs/annotations reaches the digest.
//
// Kept identical to the JS (non-unicode) regex semantics:
// - `\b` is ASCII-only `(?-u:\b)`, `\d` is `[0-9]`, the `i` flag is ASCII-only `(?i-u:...)`
//   (Rust's Unicode `(?i)` would let `k` match U+212A KELVIN SIGN; JS does not).
// - JS `\s` is spelled out: it includes U+FEFF and excludes U+0085, unlike Rust's `\s`.
// - The phone rule needs lookbehind/lookahead, which `regex` lacks, so it alone uses fancy-regex.
use std::borrow::Cow;
use std::sync::LazyLock;

const JS_SPACE: &str = r"\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";

enum Re {
    Std(regex::Regex),
    Fancy(fancy_regex::Regex),
}

struct Rule {
    re: Re,
    to: &'static str,
}

fn build() -> Result<Vec<Rule>, String> {
    let std = |p: &str, to| {
        regex::Regex::new(p)
            .map(|r| Rule { re: Re::Std(r), to })
            .map_err(|e| e.to_string())
    };
    let key_value = format!(
        r#"(?i-u:(password|passwd|pwd|token|secret|authorization|api[_-]?key))(["']?[{s}]*[:=][{s}]*["']?)(?:(?i-u:bearer)[{s}]+|(?i-u:basic)[{s}]+)?[^{s}"',;}}&]+"#,
        s = JS_SPACE
    );
    Ok(vec![
        std(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "***")?,
        std(r"(?-u:\b)[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}(?-u:\b)", "***")?,
        std(r"(?-u:\b)eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}(?:\.[A-Za-z0-9_-]*)?", "***")?,
        std(&key_value, "${1}${2}***")?,
        Rule {
            re: Re::Fancy(
                fancy_regex::Regex::new(r"(?<![0-9])(?:\+?82[-. ]?0?|0)1[016789][-. ]?[0-9]{3,4}[-. ]?[0-9]{4}(?![0-9])")
                    .map_err(|e| e.to_string())?,
            ),
            to: "***",
        },
        std(r"(?-u:\b)[A-Za-z0-9_-]{32,}(?-u:\b)", "***")?,
    ])
}

static RULES: LazyLock<Result<Vec<Rule>, String>> = LazyLock::new(build);

pub fn mask(text: &str) -> String {
    // Patterns are constants verified by tests; if one ever failed to build or match, mask everything rather than leak.
    let Ok(rules) = &*RULES else {
        return "***".to_string();
    };
    let mut out = text.to_string();
    for rule in rules {
        let replaced = match &rule.re {
            Re::Std(re) => re.replace_all(&out, rule.to),
            Re::Fancy(re) => match re.try_replacen(&out, 0, rule.to) {
                Ok(r) => r,
                Err(_) => return "***".to_string(),
            },
        };
        if let Cow::Owned(s) = replaced {
            out = s;
        }
    }
    out
}

/// A high surrogate left alone by a JS `slice` cut. Rust strings cannot hold it, so it is stored as
/// U+E000 + (unit - 0xD800) in the Private Use Area: 1 UTF-16 unit, non-word, non-space — the same
/// as the lone surrogate for every later length count and regex. Output code maps it back
/// (`\udXXX` in dry-run JSON, U+FFFD on the wire).
// shortcut: a real U+E000..U+E3FF character in the input is treated as a lone surrogate on output; switch to a UTF-16 string type if that ever shows up.
pub const LONE_SURROGATE_BASE: u32 = 0xE000;

/// JS `truncate`: length and cut in UTF-16 code units, `…` appended when cut.
pub fn truncate(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_string();
    }
    let mut out: String = char::decode_utf16(units[..max.saturating_sub(1)].iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| {
                char::from_u32(LONE_SURROGATE_BASE + u32::from(e.unpaired_surrogate()) - 0xD800)
                    .unwrap_or('\u{FFFD}')
            })
        })
        .collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_compile() {
        assert!(RULES.as_ref().is_ok(), "{:?}", RULES.as_ref().err());
    }

    // ---- mask.test.mjs ----
    #[test]
    fn phone_numbers_hyphenated_or_not() {
        assert_eq!(mask("call 01012345678 now"), "call *** now");
        assert_eq!(mask("call 010-1234-5678 now"), "call *** now");
        assert_eq!(mask("011-234-5678"), "***");
        assert_eq!(mask("+82 10-1234-5678"), "***");
    }

    #[test]
    fn does_not_eat_ordinary_numbers() {
        assert_eq!(mask("took 1234 ms, status 503"), "took 1234 ms, status 503");
    }

    #[test]
    fn email_uuid_jwt_long_tokens() {
        assert_eq!(mask("user a.b+c@school.kr failed"), "user *** failed");
        assert_eq!(mask("id 123e4567-e89b-12d3-a456-426614174000."), "id ***.");
        assert_eq!(
            mask("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc123"),
            "jwt ***"
        );
        assert_eq!(
            mask(&format!("key {} end", "a1B2".repeat(10))),
            "key *** end"
        );
    }

    #[test]
    fn key_value_secrets() {
        assert_eq!(mask("password=hunter2 ok"), "password=*** ok");
        assert_eq!(mask(r#"{"token":"abc"}"#), r#"{"token":"***"}"#);
        assert_eq!(mask("Authorization: Bearer abcdef"), "Authorization: ***");
    }

    // ---- JS-vs-Rust regex traps (expected values taken from Node) ----
    #[test]
    fn phone_lookaround_edges() {
        assert_eq!(mask("901012345678"), "901012345678"); // digit before
        assert_eq!(mask("010123456789"), "010123456789"); // digit after, no shorter split allowed
        assert_eq!(mask("a01012345678b"), "a***b");
        assert_eq!(mask("01012345678+821012345678"), "***+***");
        assert_eq!(mask("010-123-45678"), "010-123-45678"); // trailing 5th digit blocks the match
    }

    #[test]
    fn ascii_only_case_and_boundaries() {
        assert_eq!(mask("TOKEN=x PassWord: y"), "TOKEN=*** PassWord: ***");
        assert_eq!(mask("to\u{212A}en=abc"), "to\u{212A}en=abc"); // Kelvin sign is not k
        assert_eq!(mask("pwd=\u{FEFF}abc"), "pwd=\u{FEFF}***"); // JS \s includes U+FEFF
        assert_eq!(mask("pwd=\u{85}abc"), "pwd=***"); // ...but not U+0085, so it is part of the value
        assert_eq!(mask("é123E4567-E89B-12D3-A456-426614174000é"), "é***é"); // non-ASCII is a boundary
        assert_eq!(mask("１２３"), "１２３");
    }

    #[test]
    fn truncate_counts_utf16_units() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(truncate("가나다라", 3), "가나…");
        assert_eq!(truncate("😀😀", 4), "😀😀"); // 4 units
        assert_eq!(truncate("a😀b", 3), "a\u{E03D}…"); // cut splits 😀 (D83D DE00): lone high surrogate
        assert_eq!(truncate("abc", 0), "…");
        assert_eq!(truncate("", 0), "");
    }
}
