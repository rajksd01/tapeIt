//! Google Drive: OAuth sign-in (loopback + PKCE), refresh token in the Keychain, resumable upload.
//!
//!   connect:  browser ──consent──▶ 127.0.0.1:<port>/?code=… ──▶ token endpoint ──▶ Keychain
//!   upload:   POST session ─▶ PUT 8 MB chunks (308 = keep going) ─▶ 200 {id} ─▶ share "anyone with link"
//!
//! Free tier only: scope `drive.file` (app-created files), no billing, no Google review.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};
use serde_json::Value;
use sha2::{Digest, Sha256};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&fields=id";
const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const ABOUT_URL: &str = "https://www.googleapis.com/drive/v3/about?fields=user(emailAddress)";

// One OAuth client ships inside the app; every user signs in with their own Google account.
// Set these when building: TAPEIT_GOOGLE_CLIENT_ID=… TAPEIT_GOOGLE_CLIENT_SECRET=… (a desktop secret is not secret).
const BUILT_IN_ID: Option<&str> = option_env!("TAPEIT_GOOGLE_CLIENT_ID");
const BUILT_IN_SECRET: Option<&str> = option_env!("TAPEIT_GOOGLE_CLIENT_SECRET");
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

const KEYCHAIN_SERVICE: &str = "com.screenpen.app.drive";
const KEYCHAIN_ACCOUNT: &str = "refresh-token";

// Drive requires chunks in multiples of 256 KiB.
const CHUNK_BYTES: u64 = 32 * 256 * 1024;
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
const ACCEPT_POLL: Duration = Duration::from_millis(200);
const READ_TIMEOUT: Duration = Duration::from_secs(120);
// Refresh a minute early so a token never expires mid-chunk.
const TOKEN_MARGIN_SECONDS: u64 = 60;

const HTTP_OK: u16 = 200;
const HTTP_CREATED: u16 = 201;
const HTTP_RESUME_INCOMPLETE: u16 = 308;
const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_NOT_FOUND: u16 = 404;
const HTTP_GONE: u16 = 410;

const DONE_PAGE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
<html><body style=\"font:16px -apple-system;padding:40px\">TapeIt is connected to Google Drive. You can close this tab.</body></html>";

// The access token lives only in memory; the refresh token lives in the Keychain.
static ACCESS: Mutex<Option<(String, Instant)>> = Mutex::new(None);
// The signed-in email, fetched once per sign-in.
static ACCOUNT: Mutex<Option<String>> = Mutex::new(None);

/// The OAuth "Desktop app" client from Google Cloud Console.
#[derive(Debug, Clone)]
pub struct Client {
    pub id: String,
    pub secret: String,
}

impl Client {
    /// The client built into the app, unless a developer override is set in settings.
    pub fn resolve(override_id: &str, override_secret: &str) -> Self {
        if !override_id.trim().is_empty() {
            return Client { id: override_id.into(), secret: override_secret.into() };
        }
        Client { id: BUILT_IN_ID.unwrap_or_default().into(), secret: BUILT_IN_SECRET.unwrap_or_default().into() }
    }

    pub fn is_set(&self) -> bool {
        !self.id.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Uploaded {
    pub id: String,
    pub link: String,
}

/// What the progress callback wants after each chunk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Flow {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FailureKind {
    /// Network or server trouble: retry later.
    Transient,
    /// Not signed in or the grant was revoked: retrying will not help until the user reconnects.
    SignIn,
}

/// A failed upload. `session` lets the next try resume instead of starting over.
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: String,
    pub session: Option<String>,
    pub kind: FailureKind,
}

impl Failure {
    fn transient(message: impl Into<String>, session: Option<String>) -> Self {
        Failure { message: message.into(), session, kind: FailureKind::Transient }
    }

    fn sign_in(message: impl Into<String>) -> Self {
        Failure { message: message.into(), session: None, kind: FailureKind::SignIn }
    }
}

fn agent() -> ureq::Agent {
    // 308 means "resume incomplete" here, not a redirect.
    ureq::AgentBuilder::new().redirects(0).timeout_read(READ_TIMEOUT).build()
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok());
        let parsed = hex.and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[index], parsed) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                index += 1;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes)).map_err(|error| error.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Reads one query parameter from the browser's "GET /?code=…&state=… HTTP/1.1" line.
fn query_param(request_line: &str, key: &str) -> Option<String> {
    let target = request_line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| decode(value))
}

/// Waits for the browser to hit the loopback redirect; returns its request line.
fn wait_for_redirect(listener: &TcpListener) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;

    while Instant::now() < deadline {
        let Ok((mut stream, _)) = listener.accept() else {
            thread::sleep(ACCEPT_POLL);
            continue;
        };
        let _ = stream.set_nonblocking(false);
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).map_err(|error| error.to_string())?;
        let _ = stream.write_all(DONE_PAGE.as_bytes());

        // Browsers also ask for /favicon.ico; only the redirect carries a state.
        if query_param(&line, "state").is_some() {
            return Ok(line);
        }
    }
    Err("Sign-in timed out".into())
}

fn token_request(form: &[(&str, &str)]) -> Result<Value, String> {
    match agent().post(TOKEN_URL).send_form(form) {
        Ok(response) => response.into_json().map_err(|error| error.to_string()),
        Err(ureq::Error::Status(_, response)) => Err(response.into_string().unwrap_or_default()),
        Err(error) => Err(error.to_string()),
    }
}

fn remember_access(token: &Value) -> Option<String> {
    let access = token["access_token"].as_str()?.to_string();
    let lifetime = token["expires_in"].as_u64().unwrap_or(0).saturating_sub(TOKEN_MARGIN_SECONDS);
    *ACCESS.lock().unwrap() = Some((access.clone(), Instant::now() + Duration::from_secs(lifetime)));
    Some(access)
}

/// Runs the browser sign-in and stores the refresh token. Blocks until done or timed out.
pub fn connect(client: &Client) -> Result<(), String> {
    if !client.is_set() {
        return Err("This build has no Google client. Build with TAPEIT_GOOGLE_CLIENT_ID set.".into());
    }

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = random_token()?;
    let state = random_token()?;

    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        encode(&client.id),
        encode(&redirect),
        encode(SCOPE),
        challenge(&verifier),
        state
    );
    Command::new("open").arg(&url).status().map_err(|error| error.to_string())?;

    // The state check stops another local page from injecting its own code.
    let line = wait_for_redirect(&listener)?;
    if query_param(&line, "state").as_deref() != Some(state.as_str()) {
        return Err("Sign-in state mismatch".into());
    }
    let code = query_param(&line, "code").ok_or_else(|| query_param(&line, "error").unwrap_or("Sign-in cancelled".into()))?;

    let token = token_request(&[
        ("code", &code),
        ("client_id", &client.id),
        ("client_secret", &client.secret),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect),
        ("grant_type", "authorization_code"),
    ])?;
    let refresh = token["refresh_token"].as_str().ok_or("Google sent no refresh token")?;
    set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, refresh.as_bytes()).map_err(|error| error.to_string())?;
    remember_access(&token);
    Ok(())
}

pub fn disconnect() {
    let _ = delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
    *ACCESS.lock().unwrap() = None;
    *ACCOUNT.lock().unwrap() = None;
}

/// The email of the signed-in Google account, so the user sees which Drive gets the files.
pub fn account(client: &Client) -> Option<String> {
    if let Some(email) = ACCOUNT.lock().unwrap().clone() {
        return Some(email);
    }

    let token = access_token(client).ok()?;
    let body: Value = agent().get(ABOUT_URL).set("Authorization", &format!("Bearer {token}")).call().ok()?.into_json().ok()?;
    let email = body["user"]["emailAddress"].as_str()?.to_string();
    *ACCOUNT.lock().unwrap() = Some(email.clone());
    Some(email)
}

pub fn is_connected() -> bool {
    get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).is_ok()
}

/// A valid access token, refreshed from the Keychain token when needed.
fn access_token(client: &Client) -> Result<String, Failure> {
    if let Some((token, until)) = ACCESS.lock().unwrap().clone() {
        if Instant::now() < until {
            return Ok(token);
        }
    }

    let refresh = get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|_| Failure::sign_in("Connect Google Drive"))?;
    let refresh = String::from_utf8_lossy(&refresh).to_string();
    let token = token_request(&[
        ("refresh_token", &refresh),
        ("client_id", &client.id),
        ("client_secret", &client.secret),
        ("grant_type", "refresh_token"),
    ]);

    // invalid_grant = revoked or expired grant: only a new sign-in fixes it.
    match token {
        Ok(token) => remember_access(&token).ok_or_else(|| Failure::transient("Google sent no access token", None)),
        Err(body) if body.contains("invalid_grant") => Err(Failure::sign_in("Reconnect Google Drive")),
        Err(body) => Err(Failure::transient(body, None)),
    }
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase().as_str() {
        "mp4" => "video/mp4",
        "gif" => "image/gif",
        _ => "video/quicktime",
    }
}

/// Next byte to send, from a 308 "Range: bytes=0-N" header. No header means nothing stored yet.
fn next_offset(range: Option<&str>) -> u64 {
    range
        .and_then(|value| value.rsplit('-').next())
        .and_then(|last| last.trim().parse::<u64>().ok())
        .map_or(0, |last| last + 1)
}

fn start_session(token: &str, path: &Path, total: u64) -> Result<String, Failure> {
    let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
    let response = agent()
        .post(UPLOAD_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("X-Upload-Content-Type", mime_type(path))
        .set("X-Upload-Content-Length", &total.to_string())
        .send_json(serde_json::json!({ "name": name }));

    match response {
        Ok(response) => response.header("location").map(String::from).ok_or_else(|| Failure::transient("No upload session", None)),
        Err(ureq::Error::Status(HTTP_UNAUTHORIZED, _)) => {
            *ACCESS.lock().unwrap() = None;
            Err(Failure::transient("Access token expired", None))
        }
        Err(error) => Err(Failure::transient(error.to_string(), None)),
    }
}

/// Asks Drive how many bytes of the session it already has.
fn stored_offset(session: &str, total: u64) -> Result<u64, Failure> {
    let response = agent().put(session).set("Content-Range", &format!("bytes */{total}")).send_bytes(&[]);
    match response {
        Ok(response) if response.status() == HTTP_RESUME_INCOMPLETE => Ok(next_offset(response.header("range"))),
        Ok(_) => Ok(0),
        Err(ureq::Error::Status(HTTP_NOT_FOUND | HTTP_GONE, _)) => Err(Failure::transient("Upload session expired", None)),
        Err(error) => Err(Failure::transient(error.to_string(), Some(session.into()))),
    }
}

fn share(token: &str, id: &str) -> Result<(), Failure> {
    agent()
        .post(&format!("{FILES_URL}/{id}/permissions"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "role": "reader", "type": "anyone" }))
        .map(|_| ())
        .map_err(|error| Failure::transient(format!("Uploaded, but sharing failed: {error}"), None))
}

/// Uploads (or resumes) a file, makes it viewable by link, and returns its Drive id and link.
pub fn upload(client: &Client, path: &Path, session: Option<String>, mut progress: impl FnMut(f64) -> Flow) -> Result<Uploaded, Failure> {
    let token = access_token(client)?;
    let mut file = File::open(path).map_err(|error| Failure::transient(error.to_string(), None))?;
    let total = file.metadata().map_err(|error| Failure::transient(error.to_string(), None))?.len();
    if total == 0 {
        return Err(Failure::transient("The file is empty", None));
    }

    let session = match session {
        Some(session) => session,
        None => start_session(&token, path, total)?,
    };
    let mut offset = stored_offset(&session, total)?;
    let mut buffer = vec![0u8; CHUNK_BYTES as usize];

    loop {
        let keep = |message: String| Failure::transient(message, Some(session.clone()));
        let length = CHUNK_BYTES.min(total - offset) as usize;
        file.seek(SeekFrom::Start(offset)).map_err(|error| keep(error.to_string()))?;
        file.read_exact(&mut buffer[..length]).map_err(|error| keep(error.to_string()))?;

        let range = format!("bytes {}-{}/{total}", offset, offset + length as u64 - 1);
        let response = match agent().put(&session).set("Content-Range", &range).send_bytes(&buffer[..length]) {
            Ok(response) => response,
            Err(ureq::Error::Status(HTTP_NOT_FOUND | HTTP_GONE, _)) => return Err(Failure::transient("Upload session expired", None)),
            Err(error) => return Err(keep(error.to_string())),
        };

        if response.status() == HTTP_RESUME_INCOMPLETE {
            offset = next_offset(response.header("range"));
            if progress(offset as f64 / total as f64) == Flow::Stop {
                return Err(Failure::transient("Cancelled", None));
            }
            continue;
        }
        if response.status() != HTTP_OK && response.status() != HTTP_CREATED {
            return Err(keep(format!("Drive answered {}", response.status())));
        }

        let body: Value = response.into_json().map_err(|error| Failure::transient(error.to_string(), None))?;
        let id = body["id"].as_str().ok_or_else(|| Failure::transient("Drive sent no file id", None))?.to_string();
        let _ = progress(1.0);
        share(&token, &id)?;
        return Ok(Uploaded { link: format!("https://drive.google.com/file/d/{id}/view"), id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn redirect_query_is_decoded() {
        let line = "GET /?state=abc&code=4%2F0Ab_c&scope=x HTTP/1.1\r\n";
        assert_eq!(query_param(line, "code").as_deref(), Some("4/0Ab_c"));
        assert_eq!(query_param(line, "state").as_deref(), Some("abc"));
        assert_eq!(query_param("GET /favicon.ico HTTP/1.1", "state"), None);
    }

    #[test]
    fn encode_round_trips() {
        let value = "http://127.0.0.1:5000/a b";
        assert_eq!(encode(value), "http%3A%2F%2F127.0.0.1%3A5000%2Fa%20b");
        assert_eq!(decode(&encode(value)), value);
    }

    #[test]
    fn resume_offset_from_range_header() {
        assert_eq!(next_offset(None), 0);
        assert_eq!(next_offset(Some("bytes=0-8388607")), 8_388_608);
        assert_eq!(next_offset(Some("garbage")), 0);
    }

    #[test]
    fn chunk_is_multiple_of_256k() {
        assert_eq!(CHUNK_BYTES % (256 * 1024), 0);
    }
}
