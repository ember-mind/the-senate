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
/// A concurrent start may own the lock before it can answer health probes.
const INSTANCE_START_TIMEOUT: Duration = Duration::from_secs(2);
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
    let started = Instant::now();
    let _instance_lock = loop {
        match lock_instance(&state_path) {
            Ok(lock) => break lock,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(existing) = existing_instance(&state_path) {
                    let url = browser_url(
                        existing.port,
                        &existing.token,
                        args.mission,
                        args.demo.as_deref(),
                    );
                    announce(&url, existing.port, args.no_open);
                    return Ok(());
                }
                anyhow::ensure!(
                    started.elapsed() < INSTANCE_START_TIMEOUT,
                    "another Senate instance holds the lock but is not responding"
                );
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => return Err(error.into()),
        }
    };

    if let Some(existing) = existing_instance(&state_path) {
        let url = browser_url(
            existing.port,
            &existing.token,
            args.mission,
            args.demo.as_deref(),
        );
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

    let url = browser_url(port, &token, args.mission, args.demo.as_deref());
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
struct Deadline {
    inner: TcpStream,
    until: Instant,
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request took too long",
            ));
        }
        self.inner
            .set_read_timeout(Some(remaining.min(READ_TIMEOUT)))?;
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

fn browser_url(port: u16, token: &str, mission: Option<MissionId>, demo: Option<&str>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(mission) = mission {
        query.append_pair("mission", &mission.to_string());
    }
    if let Some(demo) = demo {
        query.append_pair("demo", demo);
    }
    let query = query.finish();
    let query = if query.is_empty() {
        query
    } else {
        format!("?{query}")
    };
    format!("http://127.0.0.1:{port}/{query}#token={token}")
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = crate::exec::without_jira_credentials(std::process::Command::new(opener))
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Keep the lock file's inode: deleting it could let two processes lock
/// different files with the same name.
#[derive(Debug)]
struct InstanceLock(std::fs::File);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Concurrent fork/exec may temporarily hold an inherited descriptor.
        // Unlock explicitly instead of waiting for its last copy to close.
        #[cfg(unix)]
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

fn lock_instance(state_path: &Path) -> std::io::Result<InstanceLock> {
    if let Some(parent) = state_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(state_path.with_extension("lock"))?;
    #[cfg(unix)]
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    Ok(InstanceLock(file))
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

fn write_state_file(path: &Path, state: &InstanceState) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(state)?;
    // A private temporary file also replaces an old permissive file or a
    // symlink, without truncating its target or exposing a partial token.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
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
    fn mission(&self) -> Result<Option<MissionId>, ()> {
        let Some((_, query)) = self.path.split_once('?') else {
            return Ok(None);
        };
        let mut missions =
            url::form_urlencoded::parse(query.as_bytes()).filter(|(name, _)| name == "mission");
        let Some((_, value)) = missions.next() else {
            return Ok(None);
        };
        if missions.next().is_some() {
            return Err(());
        }
        value.parse().map(Some).map_err(|_| ())
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
    let malformed =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed HTTP request");
    let too_large =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "request head too large");
    let mut head_left = MAX_HEAD_BYTES;
    // One line of the head, within what is left of its byte budget.
    let mut next_line = |reader: &mut dyn BufRead, line: &mut String| -> std::io::Result<usize> {
        let read = reader.take(head_left).read_line(line)?;
        head_left -= read as u64;
        if head_left == 0 && !line.ends_with("\r\n") {
            return Err(too_large());
        }
        if read > 0 && !line.ends_with("\r\n") {
            return Err(malformed());
        }
        Ok(read)
    };

    let mut request_line = String::new();
    if next_line(reader, &mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.trim_end_matches("\r\n").split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    if !http_token(&method)
        || !path.starts_with('/')
        || path.contains('#')
        || path.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
        || !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1"))
        || parts.next().is_some()
    {
        return Err(malformed());
    }

    let mut headers = Vec::new();
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if next_line(reader, &mut line)? == 0 {
            return Err(malformed());
        }
        let line = line.strip_suffix("\r\n").ok_or_else(malformed)?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(too_large());
        }
        let (name, value) = line.split_once(':').ok_or_else(malformed)?;
        if !http_token(name)
            || value
                .bytes()
                .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
        {
            return Err(malformed());
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(malformed());
        }
        if [
            "host",
            "origin",
            "x-senate-token",
            "x-senate-challenge",
            "content-length",
        ]
        .iter()
        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
            && headers
                .iter()
                .any(|(previous, _): &(String, String)| previous.eq_ignore_ascii_case(name))
        {
            return Err(malformed());
        }
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(malformed());
            }
            content_length = Some(value.parse::<usize>().map_err(|_| malformed())?);
        }
        headers.push((name.to_owned(), value.to_owned()));
    }

    let content_length = content_length.unwrap_or(0);
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

fn http_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
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
    let mission = match request.mission() {
        Ok(selected) => selected.or(mission),
        Err(()) => return HttpResponse::error(400, "invalid or repeated mission parameter"),
    };
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
            let Ok(id) = decode_path_component(&path["/order/".len()..]) else {
                return HttpResponse::error(400, "invalid order id encoding");
            };
            match projection::order_detail(mission, &id) {
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

/// Path components use percent escapes, with literal `+` (unlike form queries).
fn decode_path_component(component: &str) -> Result<String, ()> {
    let mut decoded = Vec::with_capacity(component.len());
    let mut bytes = component.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next().ok_or(())?).to_digit(16).ok_or(())?;
            let low = char::from(bytes.next().ok_or(())?).to_digit(16).ok_or(())?;
            decoded.push(u8::try_from(high * 16 + low).map_err(|_| ())?);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| ())
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
        202 => "Accepted",
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
            browser_url(4123, "deadbeef", None, None),
            "http://127.0.0.1:4123/#token=deadbeef"
        );
        assert_eq!(
            browser_url(4123, "deadbeef", None, Some("review")),
            "http://127.0.0.1:4123/?demo=review#token=deadbeef"
        );
    }

    #[test]
    fn browser_url_preserves_the_selected_mission_and_encodes_demo_values() {
        let mission = MissionId::from_u128(42);
        let url = browser_url(
            4123,
            "deadbeef",
            Some(mission),
            Some("review&mission=wrong#fragment"),
        );
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.fragment(), Some("token=deadbeef"));
        let pairs = parsed.query_pairs().collect::<Vec<_>>();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "mission");
        assert_eq!(pairs[0].1, mission.to_string());
        assert_eq!(pairs[1].1, "review&mission=wrong#fragment");
    }

    #[test]
    fn invalid_mission_query_never_falls_back_to_another_campaign() {
        let mission = MissionId::from_u128(42);
        for query in [
            "mission=wrong".to_owned(),
            format!("mission={mission}&mission={mission}"),
        ] {
            let mut reader = std::io::Cursor::new(format!(
                "GET /api/world?{query} HTTP/1.1\r\nHost: 127.0.0.1:4123\r\nX-Senate-Token: test\r\n\r\n"
            ));
            let request = read_request(&mut reader).unwrap().unwrap();
            assert_eq!(route(&request, 4123, "test", Some(mission)).status, 400);
        }
    }

    #[test]
    fn state_file_creates_its_directory_and_replaces_an_old_file_privately() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new-data/world.json");
        let state = InstanceState {
            pid: 1,
            port: 4123,
            token: "test-token".to_owned(),
        };
        write_state_file(&path, &state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_state_file(&path, &state).unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let target = directory.path().join("untouched");
            std::fs::write(&target, b"original").unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            write_state_file(&path, &state).unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"original");
            assert!(
                !std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        let saved: InstanceState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.token, state.token);
    }

    #[cfg(unix)]
    #[test]
    fn instance_lock_refuses_a_second_start_and_is_released_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new-data/world.json");
        let first = lock_instance(&path).unwrap();
        assert_eq!(
            lock_instance(&path).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(first);
        let _next = lock_instance(&path).unwrap();
        assert!(path.with_extension("lock").exists());
    }

    #[test]
    fn malformed_or_ambiguous_http_heads_are_refused() {
        for request in [
            "GET /\r\n\r\n",
            "GET / HTTP/1.1 extra\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: localhost:4123\r\n",
            "GET / HTTP/1.1\r\nBroken header\r\n\r\n",
            "GET / HTTP/1.1\r\nHost : localhost:4123\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: localhost:4123\r\nHost: evil\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: invalid\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: +1\r\n\r\nx",
            "POST / HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\nx",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET / HTTP/1.1\r\nX-Bad: text\0hidden\r\n\r\n",
            "GET / HTTP/1.1\nHost: localhost:4123\n\n",
        ] {
            assert!(
                read_request(&mut std::io::Cursor::new(request)).is_err(),
                "accepted {request:?}"
            );
        }
    }

    #[test]
    fn the_header_limit_is_inclusive_and_body_bytes_are_preserved() {
        let mut request = String::from("POST /api/consul HTTP/1.1\r\nContent-Length: 3\r\n");
        for i in 1..MAX_HEADERS {
            let _ = write!(request, "X-{i}: 1\r\n");
        }
        request.push_str("\r\nabc");
        let parsed = read_request(&mut std::io::Cursor::new(request))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.headers.len(), MAX_HEADERS);
        assert_eq!(parsed.body, b"abc");
        assert_eq!(reason_phrase(202), "Accepted");
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
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut late = Deadline {
            inner: listener.accept().unwrap().0,
            until: Instant::now(),
        };
        let mut buf = [0_u8; 8];
        let error = late.read(&mut buf).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn a_blocking_read_cannot_exceed_the_whole_request_budget() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let started = Instant::now();
        let mut reader = Deadline {
            inner: listener.accept().unwrap().0,
            until: started + Duration::from_millis(30),
        };
        let error = reader.read(&mut [0_u8; 8]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn order_ids_decode_percent_escapes_without_changing_literal_plus() {
        assert_eq!(
            decode_path_component("build%2F%C3%A8+%25").unwrap(),
            "build/è+%"
        );
        for invalid in ["%", "%0", "%XX", "%FF"] {
            assert!(decode_path_component(invalid).is_err());
            let mut reader = std::io::Cursor::new(format!(
                "GET /api/order/{invalid} HTTP/1.1\r\nHost: 127.0.0.1:4123\r\nX-Senate-Token: test\r\n\r\n"
            ));
            let request = read_request(&mut reader).unwrap().unwrap();
            assert_eq!(route(&request, 4123, "test", None).status, 400);
        }
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
