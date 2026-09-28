//! The Senate's local-only HTTP server: bound to `127.0.0.1` only, guarded
//! against DNS rebinding by an exact `Host` check, and against cross-site
//! reads/writes by a per-run token and an `Origin` check on mutations. Every
//! response is built from an in-memory embedded-asset table or the mission
//! projection; nothing here reads the filesystem at request time.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cli::WorldArgs;
use crate::domain::MissionId;

use super::{assets, projection};

/// Body cap for any request; the Senate's API bodies are small JSON, never
/// uploads.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Per-connection read timeout: this is a local, single-user tool, not a
/// service that must tolerate slow clients.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Whole-request budget: a client trickling bytes cannot hold a connection
/// open past this, whatever the per-read timeout.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
/// Per-connection write timeout, so a client that stops reading cannot pin
/// a thread on a large asset.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the request line plus headers.
const MAX_HEAD_BYTES: u64 = 16 * 1024;
/// Cap on the number of header lines.
const MAX_HEADERS: usize = 64;
/// Connections served at once; further ones are closed at accept. One
/// browser tab needs a handful.
const MAX_CONNECTIONS: usize = 32;
/// How long the single-instance probe waits for an existing server to
/// answer before deciding to start a new one.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the accept loop sleeps between polls of the shutdown flag.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Serialize, Deserialize)]
struct InstanceState {
    pid: u32,
    port: u16,
    token: String,
}

/// Opens the Senate: reuses a healthy running instance, or binds a fresh
/// one and blocks until Ctrl-C/SIGTERM.
///
/// # Errors
/// Returns an error when the data directory, the state file, the listener,
/// or the shutdown-signal handlers cannot be set up.
pub fn run(args: &WorldArgs) -> anyhow::Result<()> {
    let state_path = crate::store::world_state_file()?;

    if let Some(existing) = existing_instance(&state_path) {
        let url = browser_url(existing.port, &existing.token, args.demo.as_deref());
        announce(&url, existing.port, args.no_open);
        return Ok(());
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port))?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let token = generate_token()?;

    let state = InstanceState {
        pid: std::process::id(),
        port,
        token: token.clone(),
    };
    write_state_file(&state_path, &state)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    if let Err(error) = register_shutdown_signals(&shutdown) {
        let _ = std::fs::remove_file(&state_path);
        return Err(error.into());
    }

    let url = browser_url(port, &token, args.demo.as_deref());
    announce(&url, port, args.no_open);

    let mission = args.mission;
    let token = Arc::new(token);
    let active = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let Some(slot) = ConnectionSlot::take(&active) else {
                    drop(stream);
                    continue;
                };
                let token = Arc::clone(&token);
                std::thread::spawn(move || {
                    let _slot = slot;
                    let _ = handle_connection(stream, port, &token, mission);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(_) => std::thread::sleep(ACCEPT_POLL_INTERVAL),
        }
    }

    let _ = std::fs::remove_file(&state_path);
    Ok(())
}

/// Opens the browser, or prints the link when asked not to. The link carries
/// the API token, so it is printed only for `--no-open`; a terminal log or a
/// recording of a normal start never holds a working credential.
fn announce(url: &str, port: u16, no_open: bool) {
    if no_open {
        println!("The Senate is open at {url}  (ctrl-c to close; agents keep working)");
    } else {
        open_browser(url);
        println!(
            "The Senate is open at http://127.0.0.1:{port}/ in your browser  (ctrl-c to close; agents keep working; `senate world --no-open` prints the full link)"
        );
    }
}

/// One of the `MAX_CONNECTIONS` places; released when dropped.
struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    fn take(active: &Arc<AtomicUsize>) -> Option<Self> {
        if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(active)))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A reader that refuses to read past a fixed instant.
struct Deadline<R> {
    inner: R,
    until: Instant,
}

impl<R: Read> Read for Deadline<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.until {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request took too long",
            ));
        }
        self.inner.read(buf)
    }
}

fn register_shutdown_signals(flag: &Arc<AtomicBool>) -> std::io::Result<()> {
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(flag))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(flag))?;
    Ok(())
}

fn generate_token() -> std::io::Result<String> {
    random_hex()
}

fn random_hex() -> std::io::Result<String> {
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn browser_url(port: u16, token: &str, demo: Option<&str>) -> String {
    let query = demo.map_or_else(String::new, |scenario| format!("?demo={scenario}"));
    format!("http://127.0.0.1:{port}/{query}#token={token}")
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// An already-running instance whose recorded port and token still answer
/// `/api/health`, read from the marker file this server writes on start.
fn existing_instance(state_path: &Path) -> Option<InstanceState> {
    let contents = std::fs::read(state_path).ok()?;
    let state: InstanceState = serde_json::from_slice(&contents).ok()?;
    probe_health(&state).then_some(state)
}

/// Whatever answers on the recorded port must prove it holds the recorded
/// token without ever being sent it: it hashes the token with a fresh
/// challenge. After a crash, a different process on that port learns
/// nothing and is not mistaken for the Senate.
fn probe_health(state: &InstanceState) -> bool {
    let Ok(challenge) = random_hex() else {
        return false;
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HEALTH_PROBE_TIMEOUT))
        .build()
        .into();
    let url = format!("http://127.0.0.1:{}/api/health", state.port);
    let Ok(mut response) = agent
        .get(url)
        .header("X-Senate-Challenge", challenge.as_str())
        .call()
    else {
        return false;
    };
    let Ok(body) = response.body_mut().read_json::<serde_json::Value>() else {
        return false;
    };
    body.get("proof")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|proof| token_matches(proof, &health_proof(&state.token, &challenge)))
}

fn health_proof(token: &str, challenge: &str) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"senate-health\0");
    hasher.update(token.as_bytes());
    hasher.update(b"\0");
    hasher.update(challenge.as_bytes());
    hex_encode(&hasher.finalize())
}

#[cfg(unix)]
fn write_state_file(path: &Path, state: &InstanceState) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let bytes = serde_json::to_vec(state)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_state_file(path: &Path, state: &InstanceState) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(state)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The campaign a tab asked for with `?mission=<id>`, if it names one.
    fn mission(&self) -> Option<MissionId> {
        let query = self.path.split_once('?')?.1.split('#').next()?;
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("mission="))
            .and_then(|value| value.parse().ok())
    }

    /// The path with any query string or fragment removed.
    fn route_path(&self) -> &str {
        self.path
            .split(['?', '#'])
            .next()
            .unwrap_or(self.path.as_str())
    }
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    cache_no_store: bool,
    content_security_policy: bool,
}

impl HttpResponse {
    fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value)
            .unwrap_or_else(|_| b"{\"error\":\"internal error\"}".to_vec());
        Self {
            status,
            content_type: "application/json",
            body,
            cache_no_store: true,
            content_security_policy: false,
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, &serde_json::json!({ "error": message }))
    }

    fn asset(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        let is_html = content_type.starts_with("text/html");
        Self {
            status,
            content_type,
            body,
            cache_no_store: false,
            content_security_policy: is_html,
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    port: u16,
    token: &str,
    mission: Option<MissionId>,
) -> std::io::Result<()> {
    // Accepted sockets inherit the listener's non-blocking mode on macOS and
    // the BSDs; a large asset would then be cut off at the first full send
    // buffer. Each connection has its own thread, so block.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(Deadline {
        inner: stream,
        until: Instant::now() + REQUEST_DEADLINE,
    });

    let request = match read_request(&mut reader) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(_) => {
            return write_response(&mut writer, &HttpResponse::error(400, "malformed request"));
        }
    };

    let response = route(&request, port, token, mission);
    write_response(&mut writer, &response)
}

fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<HttpRequest>> {
    let too_large =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "request head too large");
    let mut head_left = MAX_HEAD_BYTES;
    // One line of the head, within what is left of its byte budget.
    let mut next_line = |reader: &mut dyn BufRead, line: &mut String| -> std::io::Result<usize> {
        let read = reader.take(head_left).read_line(line)?;
        head_left -= read as u64;
        if head_left == 0 && !line.ends_with('\n') {
            return Err(too_large());
        }
        Ok(read)
    };

    let mut request_line = String::new();
    if next_line(reader, &mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    if method.is_empty() || path.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty request line",
        ));
    }

    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if next_line(reader, &mut line)? == 0 {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(too_large());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_owned();
            let value = value.trim().to_owned();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }

    if content_length > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request body exceeds the 64 KiB cap",
        ));
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

fn route(
    request: &HttpRequest,
    port: u16,
    token: &str,
    mission: Option<MissionId>,
) -> HttpResponse {
    let Some(host) = request.header("host") else {
        return HttpResponse::error(403, "Host header is required");
    };
    if !host_matches(host, port) {
        return HttpResponse::error(403, "Host header does not match this server");
    }

    let path = request.route_path();
    if let Some(api_path) = path.strip_prefix("/api") {
        return route_api(request, api_path, port, token, mission);
    }

    if request.method != "GET" {
        return HttpResponse::error(404, "not found");
    }
    route_asset(path)
}

fn route_api(
    request: &HttpRequest,
    api_path: &str,
    port: u16,
    token: &str,
    mission: Option<MissionId>,
) -> HttpResponse {
    if request.method == "GET"
        && api_path == "/health"
        && let Some(challenge) = request.header("x-senate-challenge")
    {
        if challenge.is_empty() || challenge.len() > 128 {
            return HttpResponse::error(400, "bad challenge");
        }
        return HttpResponse::json(
            200,
            &serde_json::json!({ "ok": true, "proof": health_proof(token, challenge) }),
        );
    }

    let Some(candidate) = request.header("x-senate-token") else {
        return HttpResponse::error(401, "missing X-Senate-Token");
    };
    if !token_matches(candidate, token) {
        return HttpResponse::error(401, "invalid token");
    }

    // Browsers always send Origin on a POST; one without it is refused too.
    if request.method == "POST"
        && !request
            .header("origin")
            .is_some_and(|origin| origin_allowed(origin, port))
    {
        return HttpResponse::error(403, "Origin header does not match this server");
    }

    // A tab opened from the terminal names its campaign; one server serves
    // every campaign, so a second `W` never shows the wrong one.
    let mission = request.mission().or(mission);
    match (request.method.as_str(), api_path) {
        ("GET", "/health") => HttpResponse::json(200, &serde_json::json!({ "ok": true })),
        ("GET", "/world") => match projection::snapshot(mission) {
            Ok(state) => HttpResponse::json(
                200,
                &serde_json::to_value(state)
                    .unwrap_or_else(|_| serde_json::json!({"error": "unrepresentable state"})),
            ),
            Err(error) => HttpResponse::error(500, &error.to_string()),
        },
        ("GET", path) if path.starts_with("/order/") => {
            match projection::order_detail(mission, &path["/order/".len()..]) {
                Ok(Some(detail)) => {
                    HttpResponse::json(200, &serde_json::to_value(detail).unwrap_or_default())
                }
                Ok(None) => HttpResponse::error(404, "no such order"),
                Err(error) => HttpResponse::error(500, &error.to_string()),
            }
        }
        ("GET", "/consul") => match projection::consul_log(mission) {
            Ok(log) => HttpResponse::json(200, &serde_json::to_value(log).unwrap_or_default()),
            Err(error) => HttpResponse::error(500, &error.to_string()),
        },
        ("POST", "/consul") => ask(request, mission),
        _ => HttpResponse::error(404, "not found"),
    }
}

fn ask(request: &HttpRequest, mission: Option<MissionId>) -> HttpResponse {
    let Some(message) = serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|body| body.get("message")?.as_str().map(ToOwned::to_owned))
    else {
        return HttpResponse::error(400, "expected {\"message\": \"...\"}");
    };
    let refused = |reason: &str| {
        HttpResponse::json(
            200,
            &serde_json::json!({ "accepted": false, "reason": reason }),
        )
    };
    match projection::ask_consul(mission, &message) {
        Ok(Ok(())) => HttpResponse::json(202, &serde_json::json!({ "accepted": true })),
        Ok(Err(projection::AskRefused::Busy)) => {
            refused("The Consul is still answering your last message.")
        }
        Ok(Err(projection::AskRefused::Stopped)) => refused(
            "The Consul's session is stopped or waiting on you; pick it up in the terminal.",
        ),
        Ok(Err(projection::AskRefused::NoCampaign)) => {
            refused("There is no campaign to ask about.")
        }
        Ok(Err(projection::AskRefused::Empty)) => refused("Write something first."),
        Err(error) => HttpResponse::error(500, &error.to_string()),
    }
}

fn route_asset(path: &str) -> HttpResponse {
    let found = if path == "/" {
        assets::index()
    } else {
        assets::lookup(path)
    };
    match found {
        Some((content_type, bytes)) => HttpResponse::asset(200, content_type, bytes.to_vec()),
        None => HttpResponse::error(404, "not found"),
    }
}

/// Guards against DNS rebinding: only an exact `127.0.0.1:<port>` or
/// `localhost:<port>` Host header is accepted, whatever a hostile page's
/// own DNS says it resolves to.
fn host_matches(host: &str, port: u16) -> bool {
    let host = host.trim();
    host.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
        || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
}

/// A mutation is only accepted from a page this server itself served.
fn origin_allowed(origin: &str, port: u16) -> bool {
    let origin = origin.trim();
    origin.eq_ignore_ascii_case(&format!("http://127.0.0.1:{port}"))
        || origin.eq_ignore_ascii_case(&format!("http://localhost:{port}"))
}

/// Constant-time token comparison: the API token is a bearer credential, so
/// comparing it byte-by-byte with early exit would leak its prefix through
/// timing.
fn token_matches(candidate: &str, expected: &str) -> bool {
    let candidate = candidate.as_bytes();
    let expected = expected.as_bytes();
    if candidate.len() != expected.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (byte_a, byte_b) in candidate.iter().zip(expected.iter()) {
        diff |= byte_a ^ byte_b;
    }
    diff == 0
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Internal Server Error",
    }
}

fn write_response(writer: &mut TcpStream, response: &HttpResponse) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason_phrase(response.status)
    );
    let _ = writeln!(head, "Content-Type: {}\r", response.content_type);
    let _ = writeln!(head, "Content-Length: {}\r", response.body.len());
    head.push_str("Connection: close\r\n");
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    if response.cache_no_store {
        head.push_str("Cache-Control: no-store\r\n");
    }
    if response.content_security_policy {
        head.push_str(
            "Content-Security-Policy: default-src 'self'; img-src 'self' data: blob:; \
style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self' blob:\r\n",
        );
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(&response.body)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_header_accepts_only_this_servers_loopback_names() {
        assert!(host_matches("127.0.0.1:4123", 4123));
        assert!(host_matches("LOCALHOST:4123", 4123));
        assert!(!host_matches("127.0.0.1:4123", 9999));
        assert!(!host_matches("evil.example:4123", 4123));
        assert!(!host_matches("127.0.0.1", 4123));
    }

    #[test]
    fn token_comparison_requires_an_exact_match() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc123", "abc124"));
        assert!(!token_matches("abc12", "abc123"));
        assert!(!token_matches("", "abc123"));
    }

    #[test]
    fn origin_check_accepts_only_this_servers_own_origin() {
        assert!(origin_allowed("http://127.0.0.1:4123", 4123));
        assert!(origin_allowed("http://localhost:4123", 4123));
        assert!(!origin_allowed("http://127.0.0.1:4123", 9999));
        assert!(!origin_allowed("https://127.0.0.1:4123", 4123));
        assert!(!origin_allowed("http://evil.example", 4123));
    }

    #[test]
    fn browser_url_puts_the_token_in_the_fragment_and_demo_before_it() {
        assert_eq!(
            browser_url(4123, "deadbeef", None),
            "http://127.0.0.1:4123/#token=deadbeef"
        );
        assert_eq!(
            browser_url(4123, "deadbeef", Some("review")),
            "http://127.0.0.1:4123/?demo=review#token=deadbeef"
        );
    }

    fn get(
        server_port: u16,
        path: &str,
        token: Option<&str>,
        host: Option<&str>,
    ) -> std::io::Result<(u16, String)> {
        use std::io::Read as _;

        let mut stream = TcpStream::connect(("127.0.0.1", server_port))?;
        let host_header = host.map_or_else(|| format!("127.0.0.1:{server_port}"), str::to_owned);
        let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\n");
        if let Some(token) = token {
            let _ = writeln!(request, "X-Senate-Token: {token}\r");
        }
        request.push_str("Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes())?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        Ok((status, text))
    }

    #[test]
    fn health_endpoint_requires_the_token_and_answers_ok() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let shutdown = Arc::new(AtomicBool::new(false));
        let token = "test-token".to_owned();

        let worker_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let token = token.clone();
                        std::thread::spawn(move || {
                            let _ = handle_connection(stream, port, &token, None);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });

        let (status, _) = get(port, "/api/health", Some("test-token"), None).unwrap();
        assert_eq!(status, 200);

        // glTF characters carry their textures as blobs that three.js
        // fetches; without `blob:` in connect-src every figure renders white.
        let (status, page) = get(port, "/", None, None).unwrap();
        assert_eq!(status, 200);
        let csp = page
            .lines()
            .find(|line| line.starts_with("Content-Security-Policy:"))
            .expect("the page carries a CSP");
        assert!(csp.contains("connect-src 'self' blob:"), "{csp}");

        let (status, _) = get(port, "/api/health", None, None).unwrap();
        assert_eq!(status, 401);

        let (status, _) = get(port, "/api/health", Some("wrong"), None).unwrap();
        assert_eq!(status, 401);

        let (status, _) = get(port, "/api/health", Some("test-token"), Some("evil:1")).unwrap();
        assert_eq!(status, 403);

        // A large asset arrives whole even when the reader is slower than
        // the writer (accepted sockets must not stay non-blocking).
        let (_, expected) = assets::lookup("/vendor/three.module.min.js").expect("vendored");
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET /vendor/three.module.min.js HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut raw).unwrap();
        let body_at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(raw.len() - body_at, expected.len());

        // The single-instance probe proves the token without sending it.
        let genuine = InstanceState {
            pid: 0,
            port,
            token: "test-token".to_owned(),
        };
        assert!(probe_health(&genuine));
        let impostor = InstanceState {
            token: "another-token".to_owned(),
            ..genuine
        };
        assert!(!probe_health(&impostor));

        // A request head past the cap is refused, not buffered.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let long = "a".repeat(usize::try_from(MAX_HEAD_BYTES).unwrap() + 10);
        write!(
            stream,
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Pad: {long}\r\n\r\n"
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut raw = String::new();
        let _ = std::io::Read::read_to_string(&mut stream, &mut raw);
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw:.40}");

        // A POST from another origin, or with none, is refused before it
        // reaches the Consul; so is a body past the cap.
        let send_post = |origin: Option<&str>, length: usize| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut head = format!(
                "POST /api/consul HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Senate-Token: test-token\r\nContent-Length: {length}\r\n"
            );
            if let Some(origin) = origin {
                let _ = write!(head, "Origin: {origin}\r\n");
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&vec![b' '; length.min(16)]).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut raw = String::new();
            let _ = std::io::Read::read_to_string(&mut stream, &mut raw);
            raw
        };
        assert!(send_post(Some("http://evil.example"), 2).starts_with("HTTP/1.1 403"));
        assert!(send_post(None, 2).starts_with("HTTP/1.1 403"));
        let own = format!("http://127.0.0.1:{port}");
        assert!(send_post(Some(&own), MAX_BODY_BYTES + 1).starts_with("HTTP/1.1 400"));

        shutdown.store(true, Ordering::Relaxed);
        handle.join().expect("server thread joins");
    }

    #[test]
    fn too_many_headers_are_refused() {
        let mut head = String::from("GET / HTTP/1.1\r\n");
        for i in 0..=MAX_HEADERS {
            let _ = write!(head, "X-{i}: 1\r\n");
        }
        head.push_str("\r\n");
        let mut reader = std::io::Cursor::new(head.into_bytes());
        assert!(read_request(&mut reader).is_err());
    }

    #[test]
    fn a_reader_past_its_deadline_stops_reading() {
        let mut late = Deadline {
            inner: std::io::Cursor::new(b"GET / HTTP/1.1\r\n".to_vec()),
            until: Instant::now(),
        };
        let mut buf = [0_u8; 8];
        let error = late.read(&mut buf).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn connection_slots_are_capped_and_released() {
        let active = Arc::new(AtomicUsize::new(0));
        let held: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| ConnectionSlot::take(&active).expect("under the cap"))
            .collect();
        assert!(ConnectionSlot::take(&active).is_none());
        drop(held);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert!(ConnectionSlot::take(&active).is_some());
    }
}
