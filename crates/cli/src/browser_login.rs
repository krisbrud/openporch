//! `porch login --provider`: a browser login through an OpenID Connect provider.
//! The CLI listens on a loopback port for the login to return (RFC 8252), on a
//! path with a random part so a page that guesses the port cannot hand it a login
//! code, and exchanges the one-time code for a session.

use std::time::Duration;

use openporch_client::{Client, Session, Url};
use rand::Rng as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// How long the CLI waits for the person to sign in.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long one connection to the listener may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest request head read.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// How a login came back to the listener.
enum Returned {
    /// With a one-time login code.
    Code(String),
    /// With an `error`.
    Error(String),
}

/// Reads a request's target, such as `/callback/x?login_code=y`, from a GET.
async fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 || head.len() + n > MAX_REQUEST_BYTES {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = std::str::from_utf8(&head).ok()?.lines().next()?;
    let mut parts = line.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target.to_owned()),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>porch login</title><p>{message}</p>\n"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Waits for the login to return to `path` on `listener`. Requests for other
/// paths, and ones without a code or an error, are answered and ignored.
async fn wait(listener: &TcpListener, path: &str) -> Result<Returned, String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("waiting for the browser: {e}"))?;
        let Ok(Some(target)) =
            tokio::time::timeout(REQUEST_TIMEOUT, request_target(&mut stream)).await
        else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != path {
            respond(&mut stream, "404 Not Found", "Not found.").await;
            continue;
        }
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(code) = param("login_code") {
            respond(
                &mut stream,
                "200 OK",
                "You are signed in to porch. You can close this window.",
            )
            .await;
            return Ok(Returned::Code(code));
        }
        if let Some(error) = param("error") {
            respond(
                &mut stream,
                "200 OK",
                "The sign-in did not complete. Return to the terminal for details.",
            )
            .await;
            return Ok(Returned::Error(error));
        }
        respond(&mut stream, "400 Bad Request", "This page expects a login.").await;
    }
}

/// Runs a browser login through `provider`: `show` is given the address the
/// person opens. Returns the session.
pub(crate) async fn login(
    client: &Client,
    provider: &str,
    invitation: Option<&str>,
    show: impl FnOnce(&Url) -> Result<(), String>,
) -> Result<Session, String> {
    let valid = !provider.is_empty()
        && provider.len() <= 63
        && provider
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(format!("`{provider}` is not a provider ID"));
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("listening for the browser on a loopback port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("listening for the browser on a loopback port: {e}"))?
        .port();
    let nonce: String = (0..16)
        .map(|_| format!("{:02x}", rand::thread_rng().r#gen::<u8>()))
        .collect();
    let path = format!("/callback/{nonce}");
    let authorize = client
        .oidc_authorize_url(provider, &format!("http://127.0.0.1:{port}{path}"))
        .map_err(|e| e.to_string())?;
    show(&authorize)?;
    let returned = tokio::time::timeout(LOGIN_TIMEOUT, wait(&listener, &path))
        .await
        .map_err(|_| "the sign-in timed out; run `porch login` again".to_owned())??;
    let code = match returned {
        Returned::Code(code) => code,
        Returned::Error(error) => {
            return Err(match error.as_str() {
                "access_denied" => "the sign-in was declined at the identity provider".into(),
                "unavailable" => "the identity provider could not be reached; try again".into(),
                _ => "the identity provider did not confirm the sign-in".into(),
            });
        }
    };
    client
        .oidc_session(provider, &code, invitation)
        .await
        .map_err(|e| match e.status() {
            Some(401) => format!("the sign-in was refused: {e}"),
            _ => e.to_string(),
        })
}
