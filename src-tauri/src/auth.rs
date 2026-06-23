//! browser sign-in (RFC 8252 loopback + PKCE).
//!
//! 1. generate a PKCE verifier/challenge and a state nonce.
//! 2. bind a loopback TCP listener on an ephemeral port.
//! 3. open the system browser to `<base>/companion/authorize?...`.
//! 4. after the user clicks "Approve", the page redirects the browser to
//!    `http://127.0.0.1:<port>/callback?code=…&state=…`; we read it here.
//! 5. exchange the code + verifier at `/api/companion/token` for the real token.
//!
//! the raw token only ever arrives over HTTPS in the exchange response — never
//! in a loopback URL — and the code is useless to a local eavesdropper without
//! the verifier this process holds.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

fn rand_b64(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// run the full sign-in and return the access token. `cancel` lets the UI abort
/// the wait (Cancel button) without leaving the listener blocked.
pub fn sign_in(base: &str, app_name: &str, cancel: &AtomicBool) -> Result<String, String> {
    let base = base.trim_end_matches('/');

    let verifier = rand_b64(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = rand_b64(16);

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let url = format!(
        "{base}/companion/authorize?port={port}&state={}&challenge={}&name={}",
        urlencoding::encode(&state),
        urlencoding::encode(&challenge),
        urlencoding::encode(app_name),
    );
    open::that(&url).map_err(|e| format!("failed to open browser: {e}"))?;

    let (code, got_state) = accept_callback(&listener, cancel)?;
    if got_state != state {
        return Err("state mismatch — sign-in was not from this app".into());
    }

    // timeouts so a hung/stalled backend can't wedge the sign-in thread after the
    // callback (every other client in the app sets them too).
    let http = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let res = http
        .post(format!("{base}/api/companion/token"))
        .json(&serde_json::json!({ "code": code, "verifier": verifier }))
        .send()
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("token exchange failed: {}", res.status()));
    }
    let body: TokenResponse = res.json().map_err(|e| e.to_string())?;
    Ok(body.token)
}

/// accept connections until we get `/callback?code=…&state=…`, then return them.
fn accept_callback(listener: &TcpListener, cancel: &AtomicBool) -> Result<(String, String), String> {
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".into());
        }
        if Instant::now() > deadline {
            return Err("sign-in timed out — please try again".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let target = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("");
                if let Some(query) = target.strip_prefix("/callback?") {
                    let (code, state) = parse_callback(query);
                    respond(&mut stream, code.is_some() && state.is_some());
                    match (code, state) {
                        (Some(code), Some(state)) => return Ok((code, state)),
                        _ => return Err("callback missing code or state".into()),
                    }
                } else {
                    respond(&mut stream, false);
                    // not the callback (e.g. favicon) — keep waiting.
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn parse_callback(query: &str) -> (Option<String>, Option<String>) {
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("");
        let decoded = urlencoding::decode(val).map(|c| c.into_owned()).ok();
        match key {
            "code" => code = decoded,
            "state" => state = decoded,
            _ => {}
        }
    }
    (code, state)
}

fn respond(stream: &mut TcpStream, ok: bool) {
    let (title, msg) = if ok {
        ("Signed in", "You're connected. You can close this tab and return to BBRR Companion.")
    } else {
        ("Sign-in error", "Something went wrong. Return to BBRR Companion and try again.")
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{background:#0b0f14;color:#e6edf3;font-family:system-ui,sans-serif;\
         display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0}}\
         div{{text-align:center;max-width:28rem;padding:2rem}}h1{{color:#e94560}}</style></head>\
         <body><div><h1>{title}</h1><p>{msg}</p></div></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
