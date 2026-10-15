//! Runs `docs/quickstart.md` as written against a dev server: every `sh` block,
//! line by line, in document order.
//!
//! A block may hold `export NAME=value`, `cat > FILE <<'EOF'` with the file's
//! lines up to `EOF`, and `porch` commands, optionally reading standard input
//! with `< FILE`. Files are written to the machine's home directory, and
//! `.openporch/` names the dev server's data directory. `OPENPORCH_URL` is the
//! dev server's address. Every command must succeed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use super::deploys::deploys;
use super::{DevServer, Machine};

/// The `sh` blocks of `text`, as lines with their line numbers.
fn blocks(text: &str) -> Vec<Vec<(usize, String)>> {
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((_, line)) = lines.next() {
        if line.trim() != "```sh" {
            continue;
        }
        let mut block = Vec::new();
        for (n, l) in lines.by_ref() {
            if l.trim_start().starts_with("```") {
                break;
            }
            block.push((n + 1, l.to_owned()));
        }
        out.push(block);
    }
    out
}

/// `line` split into words as a shell would, with single and double quotes.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    assert!(quote.is_none(), "unclosed quote in `{line}`");
    if in_word {
        out.push(word);
    }
    out
}

#[tokio::test]
async fn the_quickstart_runs_as_written() {
    let server = DevServer::start(None).await;
    let mut m = Machine::new();
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/quickstart.md"),
    )
    .unwrap();
    let (data, home) = (server._data.path().to_owned(), m.home.path().to_owned());
    let path = |name: &str| -> PathBuf {
        match name.strip_prefix(".openporch/") {
            Some(rest) => data.join(rest),
            None => home.join(name),
        }
    };
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut ran = 0;
    for block in blocks(&text) {
        let mut lines = block.into_iter();
        while let Some((n, line)) = lines.next() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(assignment) = line.strip_prefix("export ") {
                let (name, value) = assignment
                    .split_once('=')
                    .unwrap_or_else(|| panic!("line {n}: `{line}`"));
                let value = if name == "OPENPORCH_URL" {
                    server.url.clone()
                } else {
                    value.to_owned()
                };
                m.set(name, &value);
                continue;
            }
            if let Some(rest) = line.strip_prefix("cat > ") {
                let (name, marker) = rest
                    .split_once(" <<")
                    .unwrap_or_else(|| panic!("line {n}: `{line}`"));
                let marker = marker.trim_matches('\'');
                let mut content = String::new();
                for (_, l) in lines.by_ref() {
                    if l == marker {
                        break;
                    }
                    content.push_str(&l);
                    content.push('\n');
                }
                let file = path(name);
                std::fs::write(&file, content).unwrap();
                files.insert(name.to_owned(), file);
                continue;
            }
            let mut args = words(line);
            assert_eq!(
                args.first().map(String::as_str),
                Some("porch"),
                "line {n}: only `export`, `cat > FILE` and `porch` run here: `{line}`"
            );
            let mut stdin = String::new();
            if args.len() >= 2 && args[args.len() - 2] == "<" {
                let file = args.pop().unwrap();
                args.pop();
                stdin = std::fs::read_to_string(path(&file))
                    .unwrap_or_else(|e| panic!("line {n}: reading `{file}`: {e}"));
            }
            let args: Vec<String> = args[1..]
                .iter()
                .map(|a| match files.get(a) {
                    Some(f) => f.to_string_lossy().into_owned(),
                    None => a.clone(),
                })
                .collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let r = m.porch(&args, &stdin).await;
            assert_eq!(r.code, 0, "line {n}: `{line}` failed: {r:?}");
            ran += 1;
        }
    }
    assert!(ran >= 15, "only {ran} commands ran");

    // Two deployments and a rollback, all of which succeeded.
    let list: Vec<Value> = deploys(&server).await;
    assert_eq!(list.len(), 3, "{list:?}");
    assert!(list.iter().all(|d| d["status"] == "succeeded"), "{list:?}");
    assert_eq!(list[0]["set_id"], list[2]["set_id"]);
}

#[test]
fn shell_words_keep_quoted_text_together() {
    assert_eq!(
        words(r#"porch deploy --comment "first deploy" --ack 'a#b=delete'"#),
        [
            "porch",
            "deploy",
            "--comment",
            "first deploy",
            "--ack",
            "a#b=delete"
        ]
    );
}
