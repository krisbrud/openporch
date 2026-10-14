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

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn table(value: &Value, columns: &[(&str, &str)]) -> String {
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    let rows: Vec<Vec<String>> =
        std::iter::once(columns.iter().map(|(h, _)| (*h).to_owned()).collect())
            .chain(
                items
                    .iter()
                    .map(|item| columns.iter().map(|(_, k)| cell(&item[*k])).collect()),
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
