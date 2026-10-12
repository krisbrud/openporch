//! The redaction layer: replaces known secret values in text before it is
//! logged, streamed, persisted or returned in an error.
//!
//! A secret is matched as written and in the escaped forms that JSON and HCL
//! output print it in. Values shorter than [`MIN_CHARS`] are not redacted,
//! since replacing every occurrence of a short string would garble the text
//! without protecting anything guessable.

/// What a redacted value is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// The shortest value that is redacted.
pub const MIN_CHARS: usize = 4;

/// Known secret values and how to recognize them in text.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Every form of every value, longest first, so that a secret containing
    /// another is replaced whole.
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
            if v.chars().count() < MIN_CHARS {
                continue;
            }
            self.forms.extend(escaped_forms(v));
            self.forms.push(v.to_owned());
        }
        self.forms.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self.forms.dedup();
    }

    /// Whether there is nothing to redact.
    pub fn is_empty(&self) -> bool {
        self.forms.is_empty()
    }

    /// `text` with every known value replaced by [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        self.forms
            .iter()
            .fold(text.to_owned(), |t, s| t.replace(s.as_str(), REDACTED))
    }
}

/// Values are never shown, only how many forms are known.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redactor({} forms)", self.forms.len())
    }
}

/// The forms a secret takes in JSON and HCL output: escaped quotes, backslashes
/// and control characters, Go's HTML-safe `<`, `>` and `&`, and
/// HCL's `$${` and `%%{`.
fn escaped_forms(v: &str) -> Vec<String> {
    let json = serde_json::to_string(v).unwrap_or_default();
    let json = json
        .strip_prefix('"')
        .and_then(|j| j.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned();
    let html = json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let hcl = json.replace("${", "$${").replace("%{", "%%{");
    [json, html, hcl].into_iter().filter(|f| f != v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_escaped_forms() {
        let r = Redactor::new(["a<b&c\"d\\e"]);
        for text in ["a<b&c\"d\\e", r#"a<b&c\"d\\e"#, r#"a<b&c\"d\\e"#] {
            assert_eq!(r.redact(text), REDACTED, "{text}");
        }
    }

    #[test]
    fn redacts_the_longer_secret_whole() {
        let r = Redactor::new(["pass", "password-1"]);
        assert_eq!(r.redact("password-1 pass"), "[REDACTED] [REDACTED]");
    }

    #[test]
    fn ignores_short_values_and_hides_values_in_debug() {
        let r = Redactor::new(["ab", "secret-value"]);
        assert_eq!(r.redact("ab"), "ab");
        assert!(!format!("{r:?}").contains("secret"));
    }
}
