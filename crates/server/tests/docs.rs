//! Runs every request example in `docs/api.md`, `docs/security.md` and
//! `docs/drivers/opentofu.md` against a dev
//! server listening on a loopback port, in document order, and checks each response.
//!
//! An example is an `http` block with a request line, headers, a blank line and an
//! optional JSON body, followed by an `http-response` block with a status line, a
//! blank line and an optional JSON body:
//!
//! - `$NAME` in a request is replaced by a captured value. `$ADMIN_TOKEN` starts as
//!   the bootstrap administrator token.
//! - In a response, the string `"$NAME"` captures the actual value (or must equal it
//!   once captured), `"…"` matches any value, objects match when every listed field
//!   matches, and arrays match element by element; a final `"…"` element allows more.
//!
//! Set `OPENPORCH_DOCS_PRINT=1` to print every actual response.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

const FILES: &[&str] = &["api.md", "security.md", "drivers/opentofu.md"];
const WILDCARD: &str = "…";

struct Example {
    line: usize,
    request: String,
    response: Option<String>,
}

/// The `http` blocks of a document, each with the `http-response` block after it.
fn examples(text: &str) -> Vec<Example> {
    let mut blocks: Vec<(usize, String, String)> = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let Some(lang) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        let mut body = Vec::new();
        for (_, l) in lines.by_ref() {
            if l.trim_start().starts_with("```") {
                break;
            }
            body.push(l);
        }
        blocks.push((n + 1, lang.trim().to_owned(), body.join("\n")));
    }
    let mut out: Vec<Example> = Vec::new();
    for (line, lang, body) in blocks {
        match lang.as_str() {
            "http" => out.push(Example {
                line,
                request: body,
                response: None,
            }),
            "http-response" => {
                let last = out
                    .last_mut()
                    .unwrap_or_else(|| panic!("line {line}: response without a request"));
                assert!(
                    last.response.is_none(),
                    "line {line}: two responses for one request"
                );
                last.response = Some(body);
            }
            other => assert!(
                !other.starts_with("http"),
                "line {line}: unknown example block `{other}`"
            ),
        }
    }
    out
}

fn substitute(text: &str, vars: &BTreeMap<String, String>, line: usize) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let name: String = rest[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if name.is_empty() {
            out.push('$');
        } else {
            let value = vars
                .get(&name)
                .unwrap_or_else(|| panic!("line {line}: `${name}` was never captured"));
            out.push_str(value);
        }
        rest = &rest[i + 1 + name.len()..];
    }
    out.push_str(rest);
    out
}

/// Splits a block into its first line, header lines and body.
fn parts(block: &str) -> (&str, Vec<&str>, &str) {
    let (head, body) = block.split_once("\n\n").unwrap_or((block, ""));
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    (first, lines.collect(), body.trim())
}

fn matches(
    pattern: &Value,
    actual: &Value,
    vars: &mut BTreeMap<String, String>,
    path: &str,
) -> Result<(), String> {
    match pattern {
        Value::String(s) if s == WILDCARD => Ok(()),
        Value::String(s) if s.starts_with('$') => {
            let name = &s[1..];
            let text = match actual {
                Value::String(a) => a.clone(),
                other => other.to_string(),
            };
            match vars.get(name) {
                Some(known) if *known != text => Err(format!(
                    "{path}: expected ${name} = {known:?}, got {text:?}"
                )),
                Some(_) => Ok(()),
                None => {
                    vars.insert(name.to_owned(), text);
                    Ok(())
                }
            }
        }
        Value::Object(fields) => {
            let Value::Object(actual) = actual else {
                return Err(format!("{path}: expected an object, got {actual}"));
            };
            for (k, p) in fields {
                let a = actual
                    .get(k)
                    .ok_or_else(|| format!("{path}: missing field `{k}`"))?;
                matches(p, a, vars, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
        Value::Array(items) => {
            let Value::Array(actual) = actual else {
                return Err(format!("{path}: expected an array, got {actual}"));
            };
            let open = items.last() == Some(&Value::String(WILDCARD.into()));
            let items = if open {
                &items[..items.len() - 1]
            } else {
                &items[..]
            };
            if actual.len() < items.len() || (!open && actual.len() != items.len()) {
                return Err(format!(
                    "{path}: expected {} elements, got {}",
                    items.len(),
                    actual.len()
                ));
            }
            for (i, (p, a)) in items.iter().zip(actual).enumerate() {
                matches(p, a, vars, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        other if other == actual => Ok(()),
        other => Err(format!("{path}: expected {other}, got {actual}")),
    }
}

async fn run(file: &Path) {
    let text = std::fs::read_to_string(file).unwrap();
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    let examples = examples(&text);
    assert!(!examples.is_empty(), "{name} has no examples");

    let dir = tempfile::tempdir().unwrap();
    let state = openporch_server::start(openporch_server::Config::new("acme", dir.path()), None)
        .await
        .unwrap();
    let token = std::fs::read_to_string(dir.path().join("bootstrap-token")).unwrap();
    let (router, _) = openporch_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    let print = std::env::var_os("OPENPORCH_DOCS_PRINT").is_some();
    let mut vars = BTreeMap::from([("ADMIN_TOKEN".to_owned(), token.trim().to_owned())]);
    for ex in examples {
        let at = format!("{name}:{}", ex.line);
        let request = substitute(&ex.request, &vars, ex.line);
        let (first, headers, body) = parts(&request);
        let (method, path) = first
            .split_once(' ')
            .unwrap_or_else(|| panic!("{at}: expected `METHOD /path`"));
        // Deployments run in the background, so a read of an environment's
        // state that returns the expected status is repeated until its body
        // shows what the example expects, or for a few seconds.
        let polls = method == "GET" && path.contains("/envs/");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let mut req = client.request(method.parse().unwrap(), format!("{base}{path}"));
            for h in &headers {
                let (k, v) = h
                    .split_once(':')
                    .unwrap_or_else(|| panic!("{at}: bad header `{h}`"));
                req = req.header(k.trim(), v.trim());
            }
            if !body.is_empty() {
                let json: Value = serde_json::from_str(body)
                    .unwrap_or_else(|e| panic!("{at}: request body is not JSON: {e}"));
                req = req.json(&json);
            }
            let res = req.send().await.unwrap();
            let status = res.status().as_u16();
            let bytes = res.bytes().await.unwrap();
            let actual: Value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
            };
            if print {
                println!("{at}: {first}\n{status}\n{actual:#}\n");
            }
            let Some(response) = &ex.response else {
                // While drafting, print mode runs examples that have no response yet.
                assert!(
                    print,
                    "{at}: no `http-response` block; got {status} {actual}"
                );
                break;
            };
            let (status_line, _, expected) = parts(response);
            let want: u16 = status_line
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("{at}: bad status line `{status_line}`"));
            let mut captured = vars.clone();
            assert_eq!(status, want, "{at}: {first} returned {actual}");
            let outcome = if expected.is_empty() {
                if actual == Value::Null {
                    Ok(())
                } else {
                    Err("returned a body".to_owned())
                }
            } else {
                let pattern: Value = serde_json::from_str(expected)
                    .unwrap_or_else(|e| panic!("{at}: response body is not JSON: {e}"));
                matches(&pattern, &actual, &mut captured, "body")
            };
            match outcome {
                Ok(()) => {
                    vars = captured;
                    break;
                }
                Err(_) if polls && std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => panic!("{at}: {first}: {e}\nactual: {actual:#}"),
            }
        }
    }
}

fn docs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

#[tokio::test]
async fn api_examples_run() {
    run(&docs().join(FILES[0])).await;
}

#[tokio::test]
async fn security_examples_run() {
    run(&docs().join(FILES[1])).await;
}

#[tokio::test]
async fn opentofu_examples_run() {
    run(&docs().join(FILES[2])).await;
}

/// The DynamoDB example shows its script as HCL and inside the definition's
/// JSON; both must be the same script.
#[test]
fn opentofu_example_script_matches_its_definition() {
    let text = std::fs::read_to_string(docs().join(FILES[2])).unwrap();
    let hcl = text
        .split("```hcl\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("an hcl block");
    let start = text.find("\"script\": ").expect("a script input") + "\"script\": ".len();
    let line = text[start..].lines().next().unwrap().trim_end_matches(',');
    let script: String = serde_json::from_str(line).unwrap();
    assert_eq!(script, hcl);
}
