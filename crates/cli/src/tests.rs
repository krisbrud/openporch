//! `porch` against a dev server: configuration precedence, the `Environment
//! variables` scenario in `cli`, `GitHub device login` in `authentication`, token
//! entry, credential file permissions and tokens kept out of diagnostics.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::config::{Credentials, FileConfig};

mod deploys;
mod objects;
mod plans;
mod quickstart;
mod reference;

/// A dev server on a loopback port.
struct DevServer {
    url: String,
    admin: String,
    _data: TempDir,
}

impl DevServer {
    async fn start(github: Option<&str>) -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut config = openporch_server::Config::new("acme", data.path());
        config.github = github.map(|base| openporch_server::config::GithubConfig {
            client_id: "Iv1.cli-test".into(),
            api_url: base.to_owned(),
            web_url: base.to_owned(),
        });
        let state = openporch_server::start(config, None).await.unwrap();
        let (router, _) = openporch_server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        let admin = std::fs::read_to_string(data.path().join("bootstrap-token"))
            .unwrap()
            .trim()
            .to_owned();
        Self {
            url: format!("http://{addr}"),
            admin,
            _data: data,
        }
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let client = Client::new(&self.url, "acme", Token::new(self.admin.clone())).unwrap();
        client
            .request(openporch_client::Method::POST, path, Some(&body))
            .await
            .unwrap()
    }
}

/// A `porch` invocation's result.
#[derive(Debug)]
struct Outcome {
    code: i32,
    out: String,
    err: String,
}

/// A user's machine: a home directory and environment variables.
struct Machine {
    home: TempDir,
    env: HashMap<String, String>,
}

impl Machine {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let env = HashMap::from([(
            "HOME".to_owned(),
            home.path().to_string_lossy().into_owned(),
        )]);
        Self { home, env }
    }

    fn set(&mut self, key: &str, value: &str) -> &mut Self {
        self.env.insert(key.into(), value.into());
        self
    }

    fn dir(&self) -> std::path::PathBuf {
        self.home.path().join(".config").join("openporch")
    }

    async fn porch(&self, args: &[&str], stdin: &str) -> Outcome {
        let env = |k: &str| self.env.get(k).cloned();
        let mut input = stdin.as_bytes();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io {
            env: &env,
            stdin: &mut input,
            stdin_is_terminal: false,
            out: &mut out,
            err: &mut err,
        };
        let code = run(
            std::iter::once("porch").chain(args.iter().copied()),
            &mut io,
        )
        .await;
        Outcome {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }
}

fn assert_no_secret(outcome: &Outcome, secret: &str) {
    assert!(
        !outcome.out.contains(secret) && !outcome.err.contains(secret),
        "token printed: {outcome:?}"
    );
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn flags_win_over_environment_variables_over_the_config_file() {
    let file = FileConfig {
        url: Some("https://file.example.com".into()),
        org: Some("file-org".into()),
        app: Some("shop".into()),
        env: Some("development".into()),
    };
    let mut creds = Credentials::default();
    creds.servers.insert(
        "https://file.example.com".into(),
        Credential {
            token: "file-token".into(),
            kind: "api".into(),
            user: None,
            expires_at: None,
        },
    );
    let vars = HashMap::from([
        ("OPENPORCH_URL", "https://env.example.com"),
        ("OPENPORCH_ORG", "env-org"),
        ("OPENPORCH_TOKEN", "env-token"),
    ]);
    let all = |k: &str| vars.get(k).map(|v| (*v).to_owned());
    let none = |_: &str| None;
    let flags = Flags {
        url: Some("https://flag.example.com".into()),
        org: Some("flag-org".into()),
        token: Token::new("flag-token"),
        ..Flags::default()
    };

    let s = resolve(&flags, &all, &file, &creds);
    assert_eq!(s.url.as_deref(), Some("https://flag.example.com"));
    assert_eq!(s.org.as_deref(), Some("flag-org"));
    let (token, source) = s.token.unwrap();
    assert_eq!((token.expose(), source), ("flag-token", TokenSource::Flag));
    assert_eq!(s.app.as_deref(), Some("shop"));
    assert_eq!(s.env.as_deref(), Some("development"));

    let s = resolve(&Flags::default(), &all, &file, &creds);
    assert_eq!(s.url.as_deref(), Some("https://env.example.com"));
    assert_eq!(s.org.as_deref(), Some("env-org"));
    let (token, source) = s.token.unwrap();
    assert_eq!((token.expose(), source), ("env-token", TokenSource::Env));

    let s = resolve(&Flags::default(), &none, &file, &creds);
    assert_eq!(s.url.as_deref(), Some("https://file.example.com"));
    assert_eq!(s.org.as_deref(), Some("file-org"));
    let (token, source) = s.token.unwrap();
    assert_eq!((token.expose(), source), ("file-token", TokenSource::File));

    // A stored token belongs to its server: another URL does not get it.
    let url_only = |k: &str| (k == "OPENPORCH_URL").then(|| "https://other.example.com".to_owned());
    let s = resolve(&Flags::default(), &url_only, &file, &creds);
    assert!(s.token.is_none());
    // Empty values do not count.
    let empty = |_: &str| Some(String::new());
    let s = resolve(&Flags::default(), &empty, &file, &creds);
    assert_eq!(s.url.as_deref(), Some("https://file.example.com"));
    assert!(matches!(s.token, Some((_, TokenSource::File))));
}

/// `Environment variables` in `cli`.
#[tokio::test]
async fn environment_variables_list_the_organizations_applications() {
    let server = DevServer::start(None).await;
    for app in ["shop", "billing"] {
        server.post("/orgs/acme/apps", json!({"id": app})).await;
    }
    let mut m = Machine::new();
    m.set("OPENPORCH_URL", &server.url)
        .set("OPENPORCH_ORG", "acme")
        .set("OPENPORCH_TOKEN", &server.admin);
    let r = m.porch(&["get", "apps"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let lines: Vec<&str> = r.out.lines().collect();
    assert!(lines[0].starts_with("ID"), "{r:?}");
    assert!(lines.iter().any(|l| l.starts_with("shop ")), "{r:?}");
    assert!(lines.iter().any(|l| l.starts_with("billing ")), "{r:?}");
    assert_no_secret(&r, &server.admin);

    for format in ["json", "yaml"] {
        let r = m.porch(&["get", "apps", "-o", format], "").await;
        assert_eq!(r.code, 0, "{r:?}");
        let apps: Value = if format == "json" {
            serde_json::from_str(&r.out).unwrap()
        } else {
            serde_yaml_ng::from_str(&r.out).unwrap()
        };
        let mut ids: Vec<_> = apps
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        assert_eq!(ids, ["billing", "shop"]);
    }

    // Nothing was written: environment variables need no files.
    assert!(!m.dir().exists());
}

#[tokio::test]
async fn missing_settings_are_named() {
    let m = Machine::new();
    let r = m.porch(&["get", "apps"], "").await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("OPENPORCH_URL"), "{r:?}");
    let r = m
        .porch(&["--url", "https://x.example.com", "get", "apps"], "")
        .await;
    assert!(r.err.contains("OPENPORCH_ORG"), "{r:?}");
    let r = m
        .porch(
            &[
                "--url",
                "https://x.example.com",
                "--org",
                "acme",
                "get",
                "apps",
            ],
            "",
        )
        .await;
    assert!(r.err.contains("porch login"), "{r:?}");
    // Plain HTTP to another machine is refused before connecting.
    let r = m
        .porch(
            &[
                "--token",
                "t0ken-value",
                "--url",
                "http://porch.example.com",
                "--org",
                "acme",
                "get",
                "apps",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("plain HTTP"), "{r:?}");
    assert_no_secret(&r, "t0ken-value");
}

#[tokio::test]
async fn login_with_a_token_stores_it_owner_only() {
    let server = DevServer::start(None).await;
    server.post("/orgs/acme/apps", json!({"id": "shop"})).await;
    let mut m = Machine::new();
    let url = server.url.clone();

    // A rejected token is not stored and not echoed.
    let r = m
        .porch(
            &["login", "--token", "--url", &url, "--org", "acme"],
            "opt_wrong-token\n",
        )
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("rejected"), "{r:?}");
    assert_no_secret(&r, "opt_wrong-token");
    assert!(!m.dir().join("credentials.toml").exists());

    // The token comes from standard input, never from the command line.
    let stdin = format!("{}\n", server.admin);
    let r = m
        .porch(
            &["login", "--token", "--url", &url, "--org", "acme"],
            &stdin,
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("as bootstrap"), "{r:?}");
    assert_no_secret(&r, &server.admin);
    let creds = m.dir().join("credentials.toml");
    #[cfg(unix)]
    {
        assert_eq!(mode(&creds), 0o600);
        assert_eq!(mode(&m.dir()), 0o700);
    }
    let stored = std::fs::read_to_string(&creds).unwrap();
    assert!(stored.contains(&server.admin));
    // The config file names the server, without the token.
    let config = std::fs::read_to_string(m.dir().join("config.toml")).unwrap();
    assert!(config.contains(&url) && config.contains("acme"), "{config}");
    assert!(!config.contains(&server.admin));

    // Later commands need no flags.
    let r = m.porch(&["get", "apps"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("shop"), "{r:?}");

    // A credentials file others can read is refused, without printing it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o644)).unwrap();
        let r = m.porch(&["get", "apps"], "").await;
        assert_eq!(r.code, 1, "{r:?}");
        assert!(r.err.contains("chmod 600"), "{r:?}");
        assert_no_secret(&r, &server.admin);
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // A token from the environment wins over the stored one.
    m.set("OPENPORCH_TOKEN", "opt_env-token");
    let r = m.porch(&["get", "apps"], "").await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("AUTH-001"), "{r:?}");
    assert_no_secret(&r, "opt_env-token");
    m.env.remove("OPENPORCH_TOKEN");

    let r = m.porch(&["logout"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        !std::fs::read_to_string(&creds)
            .unwrap()
            .contains(&server.admin)
    );
    let r = m.porch(&["get", "apps"], "").await;
    assert!(r.err.contains("porch login"), "{r:?}");
}

/// The GitHub endpoints the server calls, approving every device login as
/// `octocat`.
async fn mock_github() -> String {
    #[derive(Default)]
    struct Mock {
        polls: usize,
    }
    type Shared = Arc<Mutex<Mock>>;
    let octocat = || json!({"id": 583231, "login": "octocat", "name": null, "type": "User"});
    let app = Router::new()
        .route(
            "/login/device/code",
            post(|| async {
                Json(json!({
                    "device_code": "dev-1",
                    "user_code": "ABCD-1234",
                    "verification_uri": "https://github.com/login/device",
                    "expires_in": 900,
                    "interval": 1,
                }))
            }),
        )
        .route(
            "/login/oauth/access_token",
            post(
                |State(m): State<Shared>, Form(form): Form<HashMap<String, String>>| async move {
                    assert_eq!(form["device_code"], "dev-1");
                    let mut m = m.lock().unwrap();
                    m.polls += 1;
                    // The first poll is still pending.
                    Json(if m.polls == 1 {
                        json!({"error": "authorization_pending", "interval": 1})
                    } else {
                        json!({"access_token": "gho_cli", "token_type": "bearer"})
                    })
                },
            ),
        )
        .route(
            "/user",
            get(move |headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer gho_cli");
                Json(octocat())
            }),
        )
        .route("/user/emails", get(|| async { Json(json!([])) }))
        .route(
            "/users/{login}",
            get(move |UrlPath(login): UrlPath<String>| async move {
                if login == "octocat" {
                    Ok(Json(octocat()))
                } else {
                    Err(StatusCode::NOT_FOUND)
                }
            }),
        )
        .with_state(Shared::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}/")
}

/// `GitHub device login` in `authentication`, through the CLI.
#[tokio::test]
async fn device_login_stores_a_session_for_the_github_user() {
    let github = mock_github().await;
    let server = DevServer::start(Some(&github)).await;
    let invitation = server
        .post(
            "/orgs/acme/invitations",
            json!({"provider": "github", "login": "octocat", "role": "member"}),
        )
        .await;
    let code = invitation["code"].as_str().unwrap();
    let m = Machine::new();

    let r = m
        .porch(
            &[
                "login",
                "--url",
                &server.url,
                "--org",
                "acme",
                "--invitation",
                code,
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    // The CLI shows the device code and URL, then the linked member.
    assert!(
        r.err.contains("https://github.com/login/device") && r.err.contains("ABCD-1234"),
        "{r:?}"
    );
    let creds: Credentials =
        toml::from_str(&std::fs::read_to_string(m.dir().join("credentials.toml")).unwrap())
            .unwrap();
    let stored = &creds.servers[&server.url];
    assert_eq!(stored.kind, "session");
    let user = stored.user.clone().unwrap();
    assert!(r.out.contains(&format!("as {user}")), "{r:?}");
    assert_no_secret(&r, &stored.token);
    assert!(!r.out.contains("gho_cli") && !r.err.contains("gho_cli"));
    #[cfg(unix)]
    assert_eq!(mode(&m.dir().join("credentials.toml")), 0o600);

    // The session belongs to the member linked to GitHub ID 583231.
    let client = Client::new(&server.url, "acme", Token::new(stored.token.clone())).unwrap();
    let me = client.current_user().await.unwrap();
    assert_eq!(me["id"], user.as_str());
    assert_eq!(me["identity"], "github:583231", "{me}");
    let r = m.porch(&["get", "apps"], "").await;
    assert_eq!(r.code, 0, "{r:?}");

    // Logging out ends the session on the server.
    let r = m.porch(&["logout"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(client.current_user().await.unwrap_err().status(), Some(401));
}

#[test]
fn the_command_line_is_well_formed() {
    use clap::CommandFactory as _;
    Cli::command().debug_assert();
}

#[tokio::test]
async fn mistyped_and_malformed_input_does_not_echo_tokens() {
    let mut m = Machine::new();
    m.set("OPENPORCH_URL", "https://porch.example.com")
        .set("OPENPORCH_ORG", "acme");
    // `--token` on `login` takes no value: the value is not echoed.
    for args in [
        &["login", "--token", "opt_SECRET123"][..],
        &["login", "--token=opt_SECRET123"][..],
        &["get", "apps", "x", "opt_SECRET123"][..],
    ] {
        let r = m.porch(args, "").await;
        assert_eq!(r.code, 2, "{r:?}");
        assert!(r.err.contains("<hidden>"), "{r:?}");
        assert_no_secret(&r, "opt_SECRET123");
    }

    // A config file the parser rejects is not quoted.
    std::fs::create_dir_all(m.dir()).unwrap();
    for text in [
        "token = \"opt_SECRET789\"\n",
        "url = \"https://x.example.com\"\norg = opt_SECRET789\n",
    ] {
        std::fs::write(m.dir().join("config.toml"), text).unwrap();
        let r = m.porch(&["get", "apps"], "").await;
        assert_eq!(r.code, 1, "{r:?}");
        assert!(
            r.err.contains("config file") && r.err.contains("line"),
            "{r:?}"
        );
        assert_no_secret(&r, "opt_SECRET789");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn credentials_are_not_written_where_others_can_write() {
    use std::os::unix::fs::PermissionsExt as _;
    let server = DevServer::start(None).await;
    let m = Machine::new();
    let shared = m.home.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let config = shared.join("config.toml");
    let stdin = format!("{}\n", server.admin);
    let r = m
        .porch(
            &[
                "login",
                "--token",
                "--url",
                &server.url,
                "--org",
                "acme",
                "--config",
                config.to_str().unwrap(),
            ],
            &stdin,
        )
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("owner-only directory"), "{r:?}");
    assert_no_secret(&r, &server.admin);
    assert!(!shared.join("credentials.toml").exists());
    assert_eq!(std::fs::read_dir(&shared).unwrap().count(), 0);
}
