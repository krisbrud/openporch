//! Printing lists as tables, JSON or YAML.

use std::io::Write;

use serde_json::Value;

/// The `-o` output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Aligned columns.
    #[default]
    Table,
    /// JSON.
    Json,
    /// YAML.
    Yaml,
}

/// Prints `value` as JSON or YAML, or, for tables, the `columns` (header and
/// field) of each item of the list `value`.
pub fn print(
    out: &mut dyn Write,
    format: Format,
    value: &Value,
    columns: &[(&str, &str)],
) -> Result<(), String> {
    let text = match format {
        Format::Json => serde_json::to_string_pretty(value).map_err(|e| e.to_string())? + "\n",
        Format::Yaml => serde_yaml_ng::to_string(value).map_err(|e| e.to_string())?,
        Format::Table => table(value, columns),
    };
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// Unix milliseconds as an RFC 3339 UTC timestamp, to the second.
pub fn timestamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn cell(key: &str, v: &Value) -> String {
    if (key == "at" || key.ends_with("_at"))
        && let Some(ms) = v.as_i64()
    {
        return timestamp(ms);
    }
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn table(value: &Value, columns: &[(&str, &str)]) -> String {
    // Paginated lists wrap their items.
    let list = ["deployments", "entries", "resources"]
        .iter()
        .find_map(|k| value.get(*k).filter(|v| v.is_array()))
        .unwrap_or(value);
    let items: Vec<&Value> = match list {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    let rows: Vec<Vec<String>> =
        std::iter::once(columns.iter().map(|(h, _)| (*h).to_owned()).collect())
            .chain(
                items
                    .iter()
                    .map(|item| columns.iter().map(|(_, k)| cell(k, &item[*k])).collect()),
            )
            .collect();
    let widths: Vec<usize> = (0..columns.len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut text = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        text.push_str(line.join("  ").trim_end());
        text.push('\n');
    }
    text
}
