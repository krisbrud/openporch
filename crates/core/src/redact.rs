//! The redaction layer: replaces known secret values in text before it is
//! logged, streamed, persisted or returned in an error.
//!
//! A secret is matched as written and in the escaped forms that JSON and HCL
//! output print it in. Every non-empty value is redacted, however short: a
//! short secret garbles the text it appears in, which is the safer failure.

/// What a redacted value is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// Known secret values and how to recognize them in text.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Every form of every value, each once.
    forms: Vec<String>,
}

impl Redactor {
    /// A redactor for `values`.
    pub fn new(values: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut r = Self::default();
        r.add(values);
        r
    }

    /// Adds values to redact.
    pub fn add(&mut self, values: impl IntoIterator<Item = impl AsRef<str>>) {
        for v in values {
            let v = v.as_ref();
            if v.is_empty() {
                continue;
            }
            self.forms.extend(escaped_forms(v));
            self.forms.push(v.to_owned());
        }
        self.forms.sort();
        self.forms.dedup();
    }

    /// Whether there is nothing to redact.
    pub fn is_empty(&self) -> bool {
        self.forms.is_empty()
    }

    /// `text` with every known value replaced by [`REDACTED`]. Matches are
    /// found in the original text and overlapping or adjacent ones merged, so
    /// no part of a secret that overlaps another is left behind.
    pub fn redact(&self, text: &str) -> String {
        let mut ranges: Vec<(usize, usize)> = self
            .forms
            .iter()
            .flat_map(|form| {
                // Every occurrence, also those overlapping each other.
                let mut found = Vec::new();
                let mut from = 0;
                while let Some(i) = text[from..].find(form.as_str()) {
                    let start = from + i;
                    found.push((start, start + form.len()));
                    from = start + text[start..].chars().next().map_or(1, char::len_utf8);
                }
                found
            })
            .collect();
        if ranges.is_empty() {
            return text.to_owned();
        }
        ranges.sort_unstable();
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        let mut i = 0;
        while i < ranges.len() {
            let (start, mut end) = ranges[i];
            while i + 1 < ranges.len() && ranges[i + 1].0 <= end {
                i += 1;
                end = end.max(ranges[i].1);
            }
            out.push_str(&text[at..start]);
            out.push_str(REDACTED);
            at = end;
            i += 1;
        }
        out.push_str(&text[at..]);
        out
    }
}

/// Values are never shown, only how many forms are known.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redactor({} forms)", self.forms.len())
    }
}

/// The forms a secret takes in JSON and HCL output: escaped quotes, backslashes
/// and control characters; Go's escaping of `<`, `>`, `&`, U+2028 and U+2029;
/// HCL's `$${` and `%%{`; and Go's and HCL's escaping together.
fn escaped_forms(v: &str) -> Vec<String> {
    let json = serde_json::to_string(v).unwrap_or_default();
    let json = json
        .strip_prefix('"')
        .and_then(|j| j.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned();
    let go = json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let hcl = |s: &str| s.replace("${", "$${").replace("%{", "%%{");
    let forms = [hcl(&json), hcl(&go), json, go];
    forms.into_iter().filter(|f| f != v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_escaped_forms() {
        let r = Redactor::new(["a<b&c\"d\\e"]);
        for text in ["a<b&c\"d\\e", r#"a<b&c\"d\\e"#, r#"a\u003cb\u0026c\"d\\e"#] {
            assert_eq!(r.redact(text), REDACTED, "{text}");
        }
        let r = Redactor::new(["x${y}%{z}"]);
        assert_eq!(r.redact("x$${y}%%{z}"), REDACTED);
        let r = Redactor::new(["a\u{2028}b\u{2029}<${c}"]);
        for text in [
            r#"a\u2028b\u2029\u003c${c}"#,
            r#"a\u2028b\u2029\u003c$${c}"#,
        ] {
            assert_eq!(r.redact(text), REDACTED, "{text}");
        }
    }

    #[test]
    fn overlapping_secrets_are_redacted_whole() {
        let r = Redactor::new(["aaaa1111", "1111bbbb"]);
        assert_eq!(r.redact("<aaaa1111bbbb>"), "<[REDACTED]>");
        let r = Redactor::new(["abab"]);
        assert_eq!(r.redact("ababab ab"), "[REDACTED] ab");
    }

    #[test]
    fn redacts_the_longer_secret_whole() {
        let r = Redactor::new(["pass", "password-1"]);
        assert_eq!(r.redact("password-1 pass"), "[REDACTED] [REDACTED]");
    }

    #[test]
    fn redacts_short_values_and_hides_values_in_debug() {
        let r = Redactor::new(["", "ab", "secret-value"]);
        assert_eq!(r.redact("ab xyz"), "[REDACTED] xyz");
        assert_eq!(r.redact("xyz"), "xyz");
        assert!(!format!("{r:?}").contains("secret"));
    }
}
