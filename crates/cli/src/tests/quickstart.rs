//! Runs `docs/quickstart.md` as written against a dev server: every `sh` block,
//! line by line, in document order. The `bash` block that starts the server is
//! not run; its environment goes through the server's own configuration loader,
//! and the dev server must match what it yields.
//!
//! A block may hold `export NAME=value`, `cat > FILE <<'EOF'` with the file's
//! lines up to `EOF`, and `porch` commands, optionally reading standard input
//! with `< FILE`. Files are written to the machine's home directory, and
//! `.openporch/` names the dev server's data directory. `OPENPORCH_URL` is the
//! dev server's address. Every command must succeed. A comment that starts
//! with `# The command the preview printed` runs the `porch` command the
//! previous command printed, as a reader would copy it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use super::deploys::deploys;
use super::{DevServer, Machine};

/// The blocks of `text` fenced as `lang`, as lines with their line numbers.
fn blocks(text: &str, lang: &str) -> Vec<Vec<(usize, String)>> {
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let Some(label) = line.trim().strip_prefix("```") else {
            continue;
        };
        assert!(
            !["shell", "console", "zsh"].contains(&label),
            "line {}: fence shell commands as `sh` so they run, or `bash` for the server",
            n + 1
        );
        if label != lang {
            // Skip the block's body, so its closing fence is not read as an opening one.
            for (_, l) in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
            }
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

/// The server configuration the documented `openporch serve` line yields, through
/// the server's own loader.
fn documented_server(text: &str) -> openporch_server::Config {
    let serve = blocks(text, "bash")
        .into_iter()
        .flatten()
        .map(|(_, l)| l)
        .find(|l| l.trim_end().ends_with("openporch serve"))
        .expect("a `bash` block starts the server with `openporch serve`");
    let vars: BTreeMap<String, String> = words(&serve)
        .iter()
        .map_while(|w| w.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    openporch_server::Config::load(None, |k| vars.get(k).cloned())
        .unwrap_or_else(|e| panic!("`{serve}`: {e}"))
}

#[tokio::test]
async fn the_quickstart_runs_as_written() {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/quickstart.md"),
    )
    .unwrap();
    let documented = documented_server(&text);
    // The dev server is organization `acme` with its data in a temporary directory
    // that `.openporch/` maps to below.
    assert_eq!(documented.org, "acme");
    assert_eq!(documented.data_dir, PathBuf::from(".openporch"));
    let documented_url = format!("http://{}", documented.listen);
    let server = DevServer::start(None).await;
    let mut m = Machine::new();
    let (data, home) = (server._data.path().to_owned(), m.home.path().to_owned());
    let path = |name: &str| -> PathBuf {
        match name.strip_prefix(".openporch/") {
            Some(rest) => data.join(rest),
            None => home.join(name),
        }
    };
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut ran = 0;
    // What the previous command printed, and whether the deployments were
    // checked before the application is deleted.
    let mut printed = String::new();
    let mut checked = false;
    for block in blocks(&text, "sh") {
        let mut lines = block.into_iter();
        while let Some((n, line)) = lines.next() {
            let line = line.trim();
            let copied;
            let line = if line.starts_with("# The command the preview printed") {
                copied = printed
                    .lines()
                    .map(str::trim)
                    .rfind(|l| l.starts_with("porch "))
                    .unwrap_or_else(|| panic!("line {n}: no command was printed: {printed}"))
                    .to_owned();
                copied.as_str()
            } else {
                line
            };
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(assignment) = line.strip_prefix("export ") {
                let (name, value) = assignment
                    .split_once('=')
                    .unwrap_or_else(|| panic!("line {n}: `{line}`"));
                let value = if name == "OPENPORCH_URL" {
                    assert_eq!(value, documented_url, "line {n}: the server's address");
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
            if args.starts_with(&["delete", "app"]) && !checked {
                check_deployments(&server).await;
                checked = true;
            }
            let r = m.porch(&args, &stdin).await;
            assert_eq!(r.code, 0, "line {n}: `{line}` failed: {r:?}");
            printed = format!("{}{}", r.out, r.err);
            ran += 1;
        }
    }
    assert!(ran >= 15, "only {ran} commands ran");
    assert!(checked, "the quickstart deletes the application");
    // The application goes, with everything its deployments created, once
    // its deletion has run in the background.
    let mut apps = Value::Null;
    for _ in 0..2000 {
        let r = m.porch(&["get", "apps", "-o", "json"], "").await;
        assert_eq!(r.code, 0, "{r:?}");
        apps = serde_json::from_str(&r.out).unwrap();
        if apps == serde_json::json!([]) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(apps, serde_json::json!([]));
}

/// Two deployments and a rollback, all of which succeeded.
async fn check_deployments(server: &DevServer) {
    let list: Vec<Value> = deploys(server).await;
    assert_eq!(list.len(), 3, "{list:?}");
    assert!(list.iter().all(|d| d["status"] == "succeeded"), "{list:?}");
    assert_ne!(
        list[1]["set_id"], list[2]["set_id"],
        "the change made a new set"
    );
    assert_eq!(list[0]["set_id"], list[2]["set_id"], "the rollback");
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
