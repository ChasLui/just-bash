//! Minimal, dependency-free networking support for the `curl` builtin.
//!
//! # Threat model
//!
//! Scripts executed by this runtime are untrusted, and the runtime has no network
//! surface by default. Networking is therefore **off by default and fails closed**.
//! Only two paths may ever succeed:
//!
//! 1. a deterministic in-memory mock table (`URL -> body`) supplied by the embedder,
//!    used by tests and never touching the operating system;
//! 2. a real plain-HTTP fetch, and only when the embedder explicitly enables network
//!    access for the call.
//!
//! HTTPS/TLS is out of scope: any non-`http` scheme is rejected with
//! [`CurlError::UnsupportedScheme`] *before* any I/O happens.
//!
//! # Guarantees
//!
//! * No dependencies: `std` only.
//! * No `unsafe`.
//! * Deterministic: no clocks, randomness, environment lookups, or redirect following.
//! * Explicit limits: the response body is capped at [`MAX_RESPONSE_BYTES`], the header
//!   section at a fixed byte budget, and connect/read/write operations carry timeouts.
//! * No panics on hostile input: bytes are decoded lossily, all parsing returns
//!   [`CurlError`], and size arithmetic is bounded before allocation.
//!
//! # Layout
//!
//! * [`parse_curl_args`] / [`effective_method`] / [`urlencode_form`] are pure and do no I/O;
//!   they translate a `curl` command line into a [`CurlArgs`].
//! * [`perform`] executes an [`HttpRequest`] and returns an [`HttpResponse`].

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Maximum accepted response body size, in bytes.
///
/// Applies to both the mock path and a real fetch. Exceeding it yields
/// [`CurlError::TooLarge`] instead of unbounded memory growth.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Maximum time allowed for a TCP connect attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time allowed for a single socket read or write.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum accepted size of the response header section, in bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Maximum accepted size of a single status/header line, in bytes.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// Maximum accepted number of response headers.
const MAX_HEADERS: usize = 128;

/// Port used when the URL does not spell one out.
const DEFAULT_HTTP_PORT: u16 = 80;

/// A single HTTP request, fully resolved by the caller.
///
/// `body` is already final: any `@file` indirection and `--data-urlencode` encoding is
/// the caller's responsibility (see [`urlencode_form`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Request method, for example `"GET"` or `"POST"`.
    pub method: String,
    /// Absolute URL; only `http://` (or scheme-less, treated as HTTP) is supported.
    pub url: String,
    /// Request headers, emitted in order after the generated `Host`/`Connection` lines.
    pub headers: Vec<(String, String)>,
    /// Optional request body.
    pub body: Option<String>,
}

/// A parsed HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// Numeric status code.
    pub status: u16,
    /// Reason phrase exactly as received (for example `"OK"`).
    pub reason: String,
    /// Response headers in wire order, with original name casing.
    pub headers: Vec<(String, String)>,
    /// Response body, decoded lossily as UTF-8.
    pub body: String,
}

/// Failure modes of [`perform`], mirroring `curl`'s process exit statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurlError {
    /// Network access was not explicitly enabled by the embedder for this call.
    NetworkDisabled,
    /// The URL used a scheme other than `http` (HTTPS/TLS included). Carries the scheme.
    UnsupportedScheme(String),
    /// The target could not be reached: URL malformed, DNS failure, refused connection,
    /// or an HTTP framing error. Carries a human-readable detail.
    Connect(String),
    /// The response exceeded the configured size cap. Carries that cap.
    TooLarge {
        /// The limit that was exceeded, in bytes.
        limit: usize,
    },
    /// A connect or read/write operation exceeded its deadline.
    Timeout,
}

impl CurlError {
    /// curl-compatible process exit status (e.g. 1 unsupported protocol, 6 could-not-resolve,
    /// 7 failed-to-connect, 28 timeout).
    ///
    /// [`Connect`](CurlError::Connect) always reports 7: DNS failures are surfaced in the
    /// message text but share the "cannot reach the target" status.
    pub fn exit_code(&self) -> i32 {
        match self {
            // curl has no exact equivalent; a disabled network presents as "cannot connect".
            CurlError::NetworkDisabled => 7,
            CurlError::UnsupportedScheme(_) => 1,
            CurlError::Connect(_) => 7,
            // curl 63: "Maximum file size exceeded".
            CurlError::TooLarge { .. } => 63,
            CurlError::Timeout => 28,
        }
    }

    /// A `curl: (N) message` line WITHOUT a trailing newline for stderr.
    pub fn message(&self) -> String {
        match self {
            CurlError::NetworkDisabled => {
                format!("curl: ({}) network access is disabled", self.exit_code())
            }
            CurlError::UnsupportedScheme(scheme) => format!(
                "curl: ({}) protocol '{}' is not supported",
                self.exit_code(),
                scheme
            ),
            CurlError::Connect(detail) => {
                format!("curl: ({}) {}", self.exit_code(), detail)
            }
            CurlError::TooLarge { limit } => format!(
                "curl: ({}) response exceeded the maximum size of {} bytes",
                self.exit_code(),
                limit
            ),
            CurlError::Timeout => {
                format!("curl: ({}) operation timed out", self.exit_code())
            }
        }
    }
}

/// How a `--data*` value should be interpreted by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    /// `-d` / `--data` / `--data-raw`: taken literally.
    Raw,
    /// `--data-binary`: binary payload, no newline translation.
    Binary,
    /// `--data-urlencode`: percent-encode via [`urlencode_form`] before sending.
    UrlEncode,
}

/// The parsed form of a `curl` command line (arguments after the command word).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurlArgs {
    /// The single URL operand.
    pub url: String,
    /// Method from `-X`/`--request`, if given.
    pub method: Option<String>,
    /// Headers from `-H`/`--header`, in order.
    pub headers: Vec<(String, String)>,
    /// Request bodies in order, still unresolved (`@file` handling and URL-encoding are
    /// performed by the caller, using [`urlencode_form`] for [`DataKind::UrlEncode`]).
    pub data: Vec<(DataKind, String)>,
    /// `-o`/`--output`; the caller writes the file.
    pub output: Option<String>,
    /// `-i`/`--include`: the caller should print response headers too.
    pub include_headers: bool,
    /// `-I`/`--head`: issue a HEAD request.
    pub head_only: bool,
    /// `-s`/`--silent`: the caller should suppress progress/error noise.
    pub silent: bool,
    /// `-f`/`--fail`: the caller should turn 4xx/5xx into a failure.
    pub fail_on_http_error: bool,
    /// `-L`/`--location`: accepted for compatibility; redirects are never followed.
    pub location: bool,
}

/// Parse a curl command line (args AFTER the command word).
///
/// Unknown options are an error string beginning with `curl: `.
/// Handle: -s -i -I -f -L -o FILE -X METHOD -H HEADER, -d/--data/--data-raw/--data-binary/
/// --data-urlencode VALUE, long forms, `--opt=value`, clustered short flags (e.g. -sS,
/// -s -o out), and `--` terminating option parsing. A lone `-` is not a URL.
/// Exactly one URL operand is required.
pub fn parse_curl_args(args: &[String]) -> Result<CurlArgs, String> {
    let mut parsed = CurlArgs {
        url: String::new(),
        method: None,
        headers: Vec::new(),
        data: Vec::new(),
        output: None,
        include_headers: false,
        head_only: false,
        silent: false,
        fail_on_http_error: false,
        location: false,
    };
    let mut operands: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        index += 1;

        if options_done {
            operands.push(arg.clone());
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        // A lone `-` is conventionally stdin; this runtime has no such operand.
        if arg == "-" {
            return Err("curl: '-' is not a supported URL operand".to_string());
        }

        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            let mut take_value = |what: &str| -> Result<String, String> {
                match inline.clone() {
                    Some(value) => Ok(value),
                    None => match args.get(index) {
                        Some(value) => {
                            index += 1;
                            Ok(value.clone())
                        }
                        None => Err(format!("curl: option --{what}: requires a value")),
                    },
                }
            };
            match name {
                "silent" => parsed.silent = flag_only(name, inline.is_some())?,
                "include" => parsed.include_headers = flag_only(name, inline.is_some())?,
                "head" => parsed.head_only = flag_only(name, inline.is_some())?,
                "fail" => parsed.fail_on_http_error = flag_only(name, inline.is_some())?,
                "location" => parsed.location = flag_only(name, inline.is_some())?,
                // Accepted for compatibility; error reporting is the caller's job.
                "show-error" => {
                    flag_only(name, inline.is_some())?;
                }
                "output" => parsed.output = Some(take_value(name)?),
                "request" => parsed.method = Some(take_value(name)?),
                "header" => parsed.headers.push(split_header(&take_value(name)?)),
                "data" => parsed.data.push((DataKind::Raw, take_value(name)?)),
                "data-raw" => parsed.data.push((DataKind::Raw, take_value(name)?)),
                "data-binary" => parsed.data.push((DataKind::Binary, take_value(name)?)),
                "data-urlencode" => parsed.data.push((DataKind::UrlEncode, take_value(name)?)),
                other => return Err(format!("curl: unknown option: --{other}")),
            }
            continue;
        }

        if let Some(cluster) = arg.strip_prefix('-') {
            let flags: Vec<char> = cluster.chars().collect();
            let mut position = 0usize;
            while position < flags.len() {
                let flag = flags[position];
                position += 1;
                match flag {
                    's' => parsed.silent = true,
                    'i' => parsed.include_headers = true,
                    'I' => parsed.head_only = true,
                    'f' => parsed.fail_on_http_error = true,
                    'L' => parsed.location = true,
                    // curl's `-S` (--show-error) is accepted and has no effect here.
                    'S' => {}
                    'o' | 'X' | 'H' | 'd' => {
                        let attached: String = flags[position..].iter().collect();
                        let value = if attached.is_empty() {
                            match args.get(index) {
                                Some(value) => {
                                    index += 1;
                                    value.clone()
                                }
                                None => {
                                    return Err(format!("curl: option -{flag}: requires a value"));
                                }
                            }
                        } else {
                            attached
                        };
                        match flag {
                            'o' => parsed.output = Some(value),
                            'X' => parsed.method = Some(value),
                            'H' => parsed.headers.push(split_header(&value)),
                            _ => parsed.data.push((DataKind::Raw, value)),
                        }
                        position = flags.len();
                    }
                    other => return Err(format!("curl: unknown option: -{other}")),
                }
            }
            continue;
        }

        operands.push(arg.clone());
    }

    match operands.len() {
        0 => Err("curl: no URL specified".to_string()),
        1 => {
            parsed.url = operands.remove(0);
            Ok(parsed)
        }
        _ => Err(format!(
            "curl: more than one URL specified ('{}' and '{}')",
            operands[0], operands[1]
        )),
    }
}

/// Reject `--flag=value` for a boolean option.
fn flag_only(name: &str, has_value: bool) -> Result<bool, String> {
    if has_value {
        Err(format!("curl: option --{name}: does not take a value"))
    } else {
        Ok(true)
    }
}

/// Split a `-H` operand into `(name, value)`.
///
/// A missing colon yields an empty value rather than an error, matching curl's lenient
/// handling of empty headers.
fn split_header(spec: &str) -> (String, String) {
    match spec.split_once(':') {
        Some((name, value)) => (name.trim().to_string(), value.trim().to_string()),
        None => (spec.trim().to_string(), String::new()),
    }
}

/// Explicit -X wins; else POST when data is present; else GET. HEAD wins when
/// head_only (unless -X given).
pub fn effective_method(args: &CurlArgs) -> String {
    if let Some(method) = &args.method {
        let trimmed = method.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if args.head_only {
        return "HEAD".to_string();
    }
    if !args.data.is_empty() {
        return "POST".to_string();
    }
    "GET".to_string()
}

/// Percent-encode `key=value` pairs for --data-urlencode (RFC 3986 unreserved set).
///
/// The `=` separating the pair is preserved; everything outside `ALPHA / DIGIT /
/// "-" / "." / "_" / "~"` is encoded as `%XX` per byte (so UTF-8 survives). Text without
/// an `=` is encoded whole, matching `--data-urlencode <content>`.
pub fn urlencode_form(text: &str) -> String {
    match text.split_once('=') {
        Some((key, value)) => format!("{}={}", percent_encode(key), percent_encode(value)),
        None => percent_encode(text),
    }
}

/// Percent-encode bytes outside the RFC 3986 unreserved set.
fn percent_encode(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// Perform the request.
///
/// - If `mock` contains the exact URL, return a 200 response with that body (no I/O).
/// - Else if `allow_network` is false, return Err(NetworkDisabled).
/// - Else perform real plain HTTP over std::net::TcpStream to host:port (default 80).
///
/// Never follows redirects. Caps the response body at [`MAX_RESPONSE_BYTES`] with
/// [`CurlError::TooLarge`]. Applies connect/read timeouts.
pub fn perform(
    request: &HttpRequest,
    mock: &BTreeMap<String, String>,
    allow_network: bool,
) -> Result<HttpResponse, CurlError> {
    // 1. Deterministic mock path: an exact URL match resolves without touching the OS.
    if let Some(body) = mock.get(&request.url) {
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(CurlError::TooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }
        return Ok(HttpResponse {
            status: 200,
            reason: "OK".to_string(),
            headers: Vec::new(),
            body: body.clone(),
        });
    }

    // 2. Fail closed unless the embedder opted in.
    if !allow_network {
        return Err(CurlError::NetworkDisabled);
    }

    // 3. Real plain-HTTP fetch. URL and request are validated before any socket opens.
    let target = parse_target(&request.url)?;
    let wire = build_wire(&target, request)?;
    let stream = connect(&target)?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|err| map_io(err, "failed to set the read timeout"))?;
    stream
        .set_write_timeout(Some(READ_TIMEOUT))
        .map_err(|err| map_io(err, "failed to set the write timeout"))?;
    send_request(stream, wire, &request.method)
}

/// A validated absolute HTTP target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// Host used for name resolution (no brackets, no port).
    host: String,
    /// Value for the `Host:` header (bracketed IPv6 literal and non-default port included).
    host_header: String,
    /// TCP port.
    port: u16,
    /// Origin-form request target including any query string.
    path: String,
}

/// Parse `http://host[:port]/path?query` without a URL crate.
///
/// Rejects userinfo (`@`) and any non-`http` scheme; a URL without a scheme is treated as
/// HTTP for curl compatibility. Whitespace and control bytes are rejected to prevent
/// request-line/header injection.
fn parse_target(url: &str) -> Result<Target, CurlError> {
    if url.is_empty() {
        return Err(CurlError::Connect("empty URL".to_string()));
    }
    if url.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
        return Err(CurlError::Connect(
            "URL contains whitespace or control characters".to_string(),
        ));
    }

    let rest = match url.find("://") {
        Some(index) => {
            let scheme = url[..index].to_ascii_lowercase();
            if scheme != "http" {
                return Err(CurlError::UnsupportedScheme(scheme));
            }
            &url[index + 3..]
        }
        None => url,
    };

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let mut path = &rest[authority_end..];
    if let Some(fragment) = path.find('#') {
        path = &path[..fragment];
    }

    if authority.is_empty() {
        return Err(CurlError::Connect("missing host in URL".to_string()));
    }
    if authority.contains('@') {
        return Err(CurlError::Connect(
            "userinfo in URL is not supported".to_string(),
        ));
    }

    let (host, port) = split_host_port(authority)?;
    let host_header = if port == DEFAULT_HTTP_PORT {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };

    Ok(Target {
        host,
        host_header,
        port,
        path,
    })
}

/// Split `host`, `host:port` or `[v6]:port` into its parts.
fn split_host_port(authority: &str) -> Result<(String, u16), CurlError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or_else(|| CurlError::Connect("unterminated IPv6 literal in URL".to_string()))?;
        let host = &rest[..close];
        if host.is_empty() {
            return Err(CurlError::Connect("missing host in URL".to_string()));
        }
        let after = &rest[close + 1..];
        if after.is_empty() {
            return Ok((host.to_string(), DEFAULT_HTTP_PORT));
        }
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| CurlError::Connect(format!("invalid authority '{authority}'")))?;
        return Ok((host.to_string(), parse_port(port)?));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(CurlError::Connect("missing host in URL".to_string()));
            }
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(CurlError::Connect(format!(
                    "invalid port in URL '{authority}'"
                )));
            }
            Ok((host.to_string(), parse_port(port)?))
        }
        None => Ok((authority.to_string(), DEFAULT_HTTP_PORT)),
    }
}

/// Parse a decimal port, rejecting 0 and out-of-range values.
fn parse_port(port: &str) -> Result<u16, CurlError> {
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(CurlError::Connect(format!("invalid port '{port}' in URL"))),
        Ok(value) => Ok(value),
    }
}

/// Open a TCP connection with a bounded connect timeout.
fn connect(target: &Target) -> Result<TcpStream, CurlError> {
    let addrs: Vec<SocketAddr> = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|err| {
            CurlError::Connect(format!("could not resolve host '{}': {err}", target.host))
        })?
        .collect();
    if addrs.is_empty() {
        return Err(CurlError::Connect(format!(
            "could not resolve host '{}'",
            target.host
        )));
    }

    let mut last_error: Option<std::io::Error> = None;
    let mut timed_out = false;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                timed_out |= is_timeout(&err);
                last_error = Some(err);
            }
        }
    }
    if timed_out {
        return Err(CurlError::Timeout);
    }
    Err(CurlError::Connect(format!(
        "failed to connect to {}:{}: {}",
        target.host,
        target.port,
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    )))
}

/// Serialize the request head and body into wire bytes.
///
/// Method and header names are restricted to RFC 7230 token bytes, and header values may
/// not contain CR/LF, so hostile arguments cannot forge additional request lines.
fn build_wire(target: &Target, request: &HttpRequest) -> Result<Vec<u8>, CurlError> {
    let method = request.method.trim();
    if method.is_empty() || !method.bytes().all(is_token_byte) {
        return Err(CurlError::Connect(format!(
            "invalid request method '{method}'"
        )));
    }
    let body = request.body.as_deref();

    let mut wire = Vec::with_capacity(256 + body.map_or(0, str::len));
    wire.extend_from_slice(format!("{method} {} HTTP/1.1\r\n", target.path).as_bytes());
    wire.extend_from_slice(format!("Host: {}\r\n", target.host_header).as_bytes());
    wire.extend_from_slice(b"Connection: close\r\n");
    for (name, value) in &request.headers {
        let name = name.trim();
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(CurlError::Connect(format!("invalid header name '{name}'")));
        }
        if value.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
            return Err(CurlError::Connect(format!(
                "invalid header value for '{name}'"
            )));
        }
        // Our own Content-Length is authoritative, so a caller-supplied one is dropped
        // rather than sent twice.
        if body.is_some() && name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        wire.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    if let Some(body) = body {
        wire.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    if let Some(body) = body {
        wire.extend_from_slice(body.as_bytes());
    }
    Ok(wire)
}

/// Write the serialized request, then read the response.
fn send_request(
    mut stream: TcpStream,
    wire: Vec<u8>,
    method: &str,
) -> Result<HttpResponse, CurlError> {
    stream
        .write_all(&wire)
        .map_err(|err| map_io(err, "write failed"))?;
    stream.flush().map_err(|err| map_io(err, "write failed"))?;

    read_response(stream, method)
}

/// Parse a response from `stream` (which must be positioned at the status line).
fn read_response(stream: TcpStream, method: &str) -> Result<HttpResponse, CurlError> {
    let mut reader = BufReader::new(stream);

    let status_line = read_line(&mut reader, MAX_LINE_BYTES)?
        .ok_or_else(|| CurlError::Connect("connection closed before a status line".to_string()))?;
    let status_text = String::from_utf8_lossy(&status_line).into_owned();
    if !status_text.starts_with("HTTP/") {
        return Err(CurlError::Connect(format!(
            "malformed status line '{status_text}'"
        )));
    }
    let mut parts = status_text.splitn(3, ' ');
    let _version = parts.next();
    let code = parts
        .next()
        .ok_or_else(|| CurlError::Connect(format!("malformed status line '{status_text}'")))?;
    let status: u16 = code
        .parse()
        .ok()
        .filter(|value| (100..=599).contains(value))
        .ok_or_else(|| CurlError::Connect(format!("invalid status code '{code}'")))?;
    let reason = parts.next().unwrap_or("").trim().to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut header_bytes = 0usize;
    // `None` means the peer closed mid-headers; tolerate it instead of erroring.
    while let Some(line) = read_line(&mut reader, MAX_LINE_BYTES)? {
        if line.is_empty() {
            break;
        }
        header_bytes += line.len() + 2;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(CurlError::TooLarge {
                limit: MAX_HEADER_BYTES,
            });
        }
        if headers.len() >= MAX_HEADERS {
            return Err(CurlError::TooLarge { limit: MAX_HEADERS });
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        match text.split_once(':') {
            Some((name, value)) => {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
            None => {
                return Err(CurlError::Connect(format!(
                    "malformed header line '{text}'"
                )));
            }
        }
    }

    let bodyless = method.eq_ignore_ascii_case("HEAD")
        || status == 204
        || status == 304
        || (100..200).contains(&status);
    if bodyless {
        return Ok(HttpResponse {
            status,
            reason,
            headers,
            body: String::new(),
        });
    }

    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
    });
    let bytes = if chunked {
        read_chunked(&mut reader)?
    } else if let Some(length) = headers.iter().find_map(|(name, value)| {
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    }) {
        if length > MAX_RESPONSE_BYTES {
            return Err(CurlError::TooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }
        let mut buffer = vec![0u8; length];
        reader
            .read_exact(&mut buffer)
            .map_err(|err| map_io(err, "truncated response body"))?;
        buffer
    } else {
        read_to_end_capped(&mut reader, MAX_RESPONSE_BYTES)?
    };

    Ok(HttpResponse {
        status,
        reason,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Decode a `Transfer-Encoding: chunked` body.
fn read_chunked<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, CurlError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let line = read_line(reader, MAX_LINE_BYTES)?
            .ok_or_else(|| CurlError::Connect("unexpected end of chunked body".to_string()))?;
        let text = String::from_utf8_lossy(&line).into_owned();
        let size_text = text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| CurlError::Connect(format!("invalid chunk size '{size_text}'")))?;
        if size == 0 {
            // Consume any trailer section.
            loop {
                match read_line(reader, MAX_LINE_BYTES)? {
                    Some(line) if line.is_empty() => break,
                    Some(_) => continue,
                    None => break,
                }
            }
            break;
        }
        // `checked_add` guards against a hostile chunk size such as `ffffffffffffffff`.
        let within_limit = out
            .len()
            .checked_add(size)
            .is_some_and(|total| total <= MAX_RESPONSE_BYTES);
        if !within_limit {
            return Err(CurlError::TooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|err| map_io(err, "truncated chunk"))?;
        out.extend_from_slice(&chunk);
        // Each chunk is followed by CRLF.
        read_line(reader, MAX_LINE_BYTES)?
            .ok_or_else(|| CurlError::Connect("truncated chunk terminator".to_string()))?;
    }
    Ok(out)
}

/// Read to EOF, refusing to grow past `limit`.
fn read_to_end_capped<R: Read>(reader: &mut R, limit: usize) -> Result<Vec<u8>, CurlError> {
    let mut out: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if out.len() + read > limit {
                    return Err(CurlError::TooLarge { limit });
                }
                out.extend_from_slice(&buffer[..read]);
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(map_io(err, "read failed")),
        }
    }
    Ok(out)
}

/// Read one CRLF-terminated line, without its terminator.
///
/// Returns `Ok(None)` when the stream ends before any byte of the line is read, and caps
/// the line length so a hostile peer cannot force unbounded allocation.
fn read_line<R: BufRead>(reader: &mut R, cap: usize) -> Result<Option<Vec<u8>>, CurlError> {
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                if line.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > cap {
                    return Err(CurlError::TooLarge { limit: cap });
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(map_io(err, "read failed")),
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(line))
}

/// Map an I/O failure, distinguishing timeouts from transport errors.
fn map_io(err: std::io::Error, what: &str) -> CurlError {
    if is_timeout(&err) {
        CurlError::Timeout
    } else {
        CurlError::Connect(format!("{what}: {err}"))
    }
}

/// Whether an I/O error represents an expired deadline.
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

/// RFC 7230 token byte (valid in a method name or header field name).
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
    }

    fn request(method: &str, url: &str) -> HttpRequest {
        HttpRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
        }
    }

    fn no_mock() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Read one HTTP request (request line, headers, declared body) from a socket.
    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(_) => break,
            }
        }
        let head_end = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .unwrap_or(buffer.len());
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buffer.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(_) => break,
            }
        }
        buffer
    }

    /// Serve exactly one request on an ephemeral loopback port, then return.
    ///
    /// The listener is non-blocking with a deadline so a failing test can never hang the
    /// suite waiting on `join`.
    fn spawn_server<F>(handler: F) -> (u16, JoinHandle<()>)
    where
        F: FnOnce(&[u8]) -> Vec<u8> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).expect("blocking");
                        let received = read_request(&mut stream);
                        let response = handler(&received);
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                        let _ = stream.shutdown(Shutdown::Write);
                        return;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });
        (port, handle)
    }

    /// The raw request head, normalised to lowercase for assertions.
    fn request_head(received: &[u8]) -> String {
        String::from_utf8_lossy(received).to_ascii_lowercase()
    }

    // ---------------------------------------------------------------- arg parsing

    #[test]
    fn parse_minimal_url() {
        let parsed = parse_curl_args(&args(&["http://example.com/"])).expect("parses");
        assert_eq!(parsed.url, "http://example.com/");
        assert!(parsed.method.is_none());
        assert!(parsed.headers.is_empty());
        assert!(parsed.data.is_empty());
        assert!(parsed.output.is_none());
        assert!(!parsed.include_headers);
        assert!(!parsed.head_only);
        assert!(!parsed.silent);
        assert!(!parsed.fail_on_http_error);
        assert!(!parsed.location);
    }

    #[test]
    fn parse_silent_short_and_long() {
        assert!(parse_curl_args(&args(&["-s", "http://x/"])).unwrap().silent);
        assert!(
            parse_curl_args(&args(&["--silent", "http://x/"]))
                .unwrap()
                .silent
        );
    }

    #[test]
    fn parse_include_short_and_long() {
        assert!(
            parse_curl_args(&args(&["-i", "http://x/"]))
                .unwrap()
                .include_headers
        );
        assert!(
            parse_curl_args(&args(&["--include", "http://x/"]))
                .unwrap()
                .include_headers
        );
    }

    #[test]
    fn parse_head_short_and_long() {
        assert!(
            parse_curl_args(&args(&["-I", "http://x/"]))
                .unwrap()
                .head_only
        );
        assert!(
            parse_curl_args(&args(&["--head", "http://x/"]))
                .unwrap()
                .head_only
        );
    }

    #[test]
    fn parse_fail_short_and_long() {
        assert!(
            parse_curl_args(&args(&["-f", "http://x/"]))
                .unwrap()
                .fail_on_http_error
        );
        assert!(
            parse_curl_args(&args(&["--fail", "http://x/"]))
                .unwrap()
                .fail_on_http_error
        );
    }

    #[test]
    fn parse_location_short_and_long() {
        assert!(
            parse_curl_args(&args(&["-L", "http://x/"]))
                .unwrap()
                .location
        );
        assert!(
            parse_curl_args(&args(&["--location", "http://x/"]))
                .unwrap()
                .location
        );
    }

    #[test]
    fn parse_output_forms() {
        assert_eq!(
            parse_curl_args(&args(&["-o", "out.txt", "http://x/"]))
                .unwrap()
                .output
                .as_deref(),
            Some("out.txt")
        );
        assert_eq!(
            parse_curl_args(&args(&["--output", "out.txt", "http://x/"]))
                .unwrap()
                .output
                .as_deref(),
            Some("out.txt")
        );
        assert_eq!(
            parse_curl_args(&args(&["--output=out.txt", "http://x/"]))
                .unwrap()
                .output
                .as_deref(),
            Some("out.txt")
        );
        assert_eq!(
            parse_curl_args(&args(&["-oout.txt", "http://x/"]))
                .unwrap()
                .output
                .as_deref(),
            Some("out.txt")
        );
    }

    #[test]
    fn parse_request_forms() {
        assert_eq!(
            parse_curl_args(&args(&["-X", "POST", "http://x/"]))
                .unwrap()
                .method
                .as_deref(),
            Some("POST")
        );
        assert_eq!(
            parse_curl_args(&args(&["--request", "DELETE", "http://x/"]))
                .unwrap()
                .method
                .as_deref(),
            Some("DELETE")
        );
        assert_eq!(
            parse_curl_args(&args(&["--request=PUT", "http://x/"]))
                .unwrap()
                .method
                .as_deref(),
            Some("PUT")
        );
        assert_eq!(
            parse_curl_args(&args(&["-XPOST", "http://x/"]))
                .unwrap()
                .method
                .as_deref(),
            Some("POST")
        );
    }

    #[test]
    fn parse_headers_preserve_order_and_split_colon() {
        let parsed = parse_curl_args(&args(&[
            "-H",
            "X-One: 1",
            "--header=X-Two: 2",
            "-HX-Three:3",
            "http://x/",
        ]))
        .unwrap();
        assert_eq!(
            parsed.headers,
            vec![
                ("X-One".to_string(), "1".to_string()),
                ("X-Two".to_string(), "2".to_string()),
                ("X-Three".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_header_without_colon_gets_empty_value() {
        let parsed = parse_curl_args(&args(&["-H", "X-Empty", "http://x/"])).unwrap();
        assert_eq!(parsed.headers, vec![("X-Empty".to_string(), String::new())]);
    }

    #[test]
    fn parse_header_value_keeps_inner_colons() {
        let parsed = parse_curl_args(&args(&["-H", "X-Time: 12:30", "http://x/"])).unwrap();
        assert_eq!(
            parsed.headers,
            vec![("X-Time".to_string(), "12:30".to_string())]
        );
    }

    #[test]
    fn parse_data_short_defaults_to_raw() {
        let parsed = parse_curl_args(&args(&["-d", "a=1", "-d", "b=2", "http://x/"])).unwrap();
        assert_eq!(
            parsed.data,
            vec![
                (DataKind::Raw, "a=1".to_string()),
                (DataKind::Raw, "b=2".to_string()),
            ]
        );
    }

    #[test]
    fn parse_data_long_forms_and_kinds() {
        let parsed = parse_curl_args(&args(&[
            "--data",
            "a",
            "--data-raw",
            "b",
            "--data-binary",
            "c",
            "--data-urlencode",
            "d=e f",
            "http://x/",
        ]))
        .unwrap();
        assert_eq!(
            parsed.data,
            vec![
                (DataKind::Raw, "a".to_string()),
                (DataKind::Raw, "b".to_string()),
                (DataKind::Binary, "c".to_string()),
                (DataKind::UrlEncode, "d=e f".to_string()),
            ]
        );
    }

    #[test]
    fn parse_data_equals_form_allows_empty_value() {
        let parsed = parse_curl_args(&args(&["--data=", "http://x/"])).unwrap();
        assert_eq!(parsed.data, vec![(DataKind::Raw, String::new())]);
    }

    #[test]
    fn parse_clustered_short_flags() {
        let parsed = parse_curl_args(&args(&["-sSLf", "http://x/"])).unwrap();
        assert!(parsed.silent && parsed.location && parsed.fail_on_http_error);
        let parsed = parse_curl_args(&args(&["-si", "http://x/"])).unwrap();
        assert!(parsed.silent && parsed.include_headers);
        let parsed = parse_curl_args(&args(&["-sS", "http://x/"])).unwrap();
        assert!(parsed.silent);
    }

    #[test]
    fn parse_cluster_final_option_takes_next_argument() {
        let parsed = parse_curl_args(&args(&["-so", "payload.bin", "http://x/"])).unwrap();
        assert!(parsed.silent);
        assert_eq!(parsed.output.as_deref(), Some("payload.bin"));
        assert_eq!(parsed.url, "http://x/");
    }

    #[test]
    fn parse_double_dash_terminates_options() {
        let parsed = parse_curl_args(&args(&["-s", "--", "-weird-url"])).unwrap();
        assert!(parsed.silent);
        assert_eq!(parsed.url, "-weird-url");
    }

    #[test]
    fn parse_lone_dash_is_not_a_url() {
        let err = parse_curl_args(&args(&["-"])).unwrap_err();
        assert!(err.starts_with("curl: "), "{err}");
        assert!(parse_curl_args(&args(&["-", "http://x/"])).is_err());
    }

    #[test]
    fn parse_missing_values_are_errors() {
        for bad in [
            args(&["-o"]),
            args(&["-X"]),
            args(&["-H"]),
            args(&["-d"]),
            args(&["--output"]),
            args(&["--request"]),
            args(&["--header"]),
            args(&["--data-urlencode"]),
            args(&["http://x/", "-o"]),
        ] {
            let err = parse_curl_args(&bad).unwrap_err();
            assert!(err.starts_with("curl: "), "{err}");
        }
    }

    #[test]
    fn parse_unknown_options_are_errors() {
        for bad in [
            args(&["-z", "http://x/"]),
            args(&["--bogus", "http://x/"]),
            args(&["-sz", "http://x/"]),
            args(&["--bogus=1", "http://x/"]),
        ] {
            let err = parse_curl_args(&bad).unwrap_err();
            assert!(err.starts_with("curl: "), "{err}");
            assert!(err.contains("unknown option"), "{err}");
        }
    }

    #[test]
    fn parse_flag_rejects_inline_value() {
        let err = parse_curl_args(&args(&["--silent=1", "http://x/"])).unwrap_err();
        assert!(err.starts_with("curl: "), "{err}");
        assert!(err.contains("does not take a value"), "{err}");
    }

    #[test]
    fn parse_requires_exactly_one_url() {
        let err = parse_curl_args(&args(&["-s"])).unwrap_err();
        assert!(err.starts_with("curl: "), "{err}");
        assert!(err.contains("no URL"), "{err}");

        let err = parse_curl_args(&args(&["http://a/", "http://b/"])).unwrap_err();
        assert!(err.starts_with("curl: "), "{err}");
        assert!(err.contains("more than one URL"), "{err}");

        let err = parse_curl_args(&[]).unwrap_err();
        assert!(err.contains("no URL"), "{err}");
    }

    #[test]
    fn parse_url_may_be_followed_by_flags() {
        let parsed = parse_curl_args(&args(&["http://x/", "-s", "-o", "f"])).unwrap();
        assert_eq!(parsed.url, "http://x/");
        assert!(parsed.silent);
        assert_eq!(parsed.output.as_deref(), Some("f"));
    }

    // ------------------------------------------------------------- method selection

    #[test]
    fn method_defaults_to_get() {
        let parsed = parse_curl_args(&args(&["http://x/"])).unwrap();
        assert_eq!(effective_method(&parsed), "GET");
    }

    #[test]
    fn method_is_post_when_data_present() {
        let parsed = parse_curl_args(&args(&["-d", "a=1", "http://x/"])).unwrap();
        assert_eq!(effective_method(&parsed), "POST");
    }

    #[test]
    fn method_is_head_when_head_only() {
        let parsed = parse_curl_args(&args(&["-I", "http://x/"])).unwrap();
        assert_eq!(effective_method(&parsed), "HEAD");
    }

    #[test]
    fn explicit_method_wins() {
        let parsed = parse_curl_args(&args(&["-d", "a=1", "-X", "PUT", "http://x/"])).unwrap();
        assert_eq!(effective_method(&parsed), "PUT");

        let parsed = parse_curl_args(&args(&["-I", "-X", "DELETE", "http://x/"])).unwrap();
        assert_eq!(effective_method(&parsed), "DELETE");

        let parsed = parse_curl_args(&args(&["-I", "-X", "", "http://x/"])).unwrap();
        assert_eq!(effective_method(&parsed), "HEAD");
    }

    // ------------------------------------------------------------------- urlencode

    #[test]
    fn urlencode_keeps_unreserved_and_separator() {
        assert_eq!(urlencode_form("a-b.c_d~e=1"), "a-b.c_d~e=1");
        assert_eq!(urlencode_form("name=value"), "name=value");
    }

    #[test]
    fn urlencode_escapes_space_and_reserved_characters() {
        assert_eq!(urlencode_form("q=hello world"), "q=hello%20world");
        assert_eq!(urlencode_form("q=a&b=c"), "q=a%26b%3Dc");
        assert_eq!(urlencode_form("q=+"), "q=%2B");
        assert_eq!(urlencode_form("q=/?#[]@"), "q=%2F%3F%23%5B%5D%40");
    }

    #[test]
    fn urlencode_without_equals_escapes_everything() {
        assert_eq!(urlencode_form("hello world"), "hello%20world");
        assert_eq!(urlencode_form(""), "");
    }

    #[test]
    fn urlencode_escapes_utf8_bytes() {
        assert_eq!(urlencode_form("k=é"), "k=%C3%A9");
    }

    // ------------------------------------------------------------------- mock path

    #[test]
    fn mock_hit_returns_200_without_network() {
        let mut mock = no_mock();
        mock.insert("http://example.com/hello".to_string(), "hi".to_string());
        let response = perform(&request("GET", "http://example.com/hello"), &mock, false).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.body, "hi");
        assert!(response.headers.is_empty());
    }

    #[test]
    fn mock_lookup_is_exact() {
        let mut mock = no_mock();
        mock.insert("http://example.com/".to_string(), "hi".to_string());
        let err = perform(&request("GET", "http://example.com"), &mock, false).unwrap_err();
        assert_eq!(err, CurlError::NetworkDisabled);
    }

    #[test]
    fn mock_oversize_response_is_too_large() {
        let mut mock = no_mock();
        mock.insert(
            "http://example.com/big".to_string(),
            "x".repeat(MAX_RESPONSE_BYTES + 1),
        );
        let err = perform(&request("GET", "http://example.com/big"), &mock, false).unwrap_err();
        assert_eq!(
            err,
            CurlError::TooLarge {
                limit: MAX_RESPONSE_BYTES
            }
        );
        assert_eq!(err.exit_code(), 63);
    }

    #[test]
    fn mock_allows_exactly_the_limit() {
        let mut mock = no_mock();
        mock.insert(
            "http://example.com/max".to_string(),
            "x".repeat(MAX_RESPONSE_BYTES),
        );
        let response = perform(&request("GET", "http://example.com/max"), &mock, false).unwrap();
        assert_eq!(response.body.len(), MAX_RESPONSE_BYTES);
    }

    // --------------------------------------------------------------- fail-closed

    #[test]
    fn network_disabled_fails_closed() {
        let err = perform(&request("GET", "http://example.com/"), &no_mock(), false).unwrap_err();
        assert_eq!(err, CurlError::NetworkDisabled);
        assert_eq!(err.exit_code(), 7);
        assert_eq!(err.message(), "curl: (7) network access is disabled");
    }

    // ------------------------------------------------------------ URL validation

    #[test]
    fn https_is_rejected_without_io() {
        let err = perform(&request("GET", "https://example.com/"), &no_mock(), true).unwrap_err();
        assert_eq!(err, CurlError::UnsupportedScheme("https".to_string()));
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.message(), "curl: (1) protocol 'https' is not supported");
    }

    #[test]
    fn other_schemes_are_rejected() {
        for (url, scheme) in [
            ("ftp://example.com/x", "ftp"),
            ("file:///etc/passwd", "file"),
            ("HTTPX://example.com/", "httpx"),
            ("ws://example.com/", "ws"),
        ] {
            let err = perform(&request("GET", url), &no_mock(), true).unwrap_err();
            assert_eq!(
                err,
                CurlError::UnsupportedScheme(scheme.to_string()),
                "{url}"
            );
        }
    }

    #[test]
    fn scheme_is_case_insensitive_for_http() {
        // Resolves to a refused connection on loopback rather than a scheme error.
        let err = perform(&request("GET", "HTTP://127.0.0.1:1/"), &no_mock(), true).unwrap_err();
        assert!(matches!(err, CurlError::Connect(_)), "{err:?}");
    }

    #[test]
    fn userinfo_is_rejected() {
        let err = perform(
            &request("GET", "http://user:pass@example.com/"),
            &no_mock(),
            true,
        )
        .unwrap_err();
        match err {
            CurlError::Connect(detail) => assert!(detail.contains("userinfo"), "{detail}"),
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn malformed_urls_are_rejected() {
        for url in [
            "",
            "http://",
            "http:///path",
            "http://example.com:abc/",
            "http://example.com:0/",
            "http://example.com:/",
            "http://exa mple.com/",
            "http://example.com/a\r\nX-Injected: 1",
            "http://[::1/",
        ] {
            let err = perform(&request("GET", url), &no_mock(), true).unwrap_err();
            assert!(
                matches!(err, CurlError::Connect(_) | CurlError::UnsupportedScheme(_)),
                "{url} -> {err:?}"
            );
        }
    }

    #[test]
    fn invalid_method_is_rejected_before_io() {
        let mut req = request("GET / HTTP/1.1", "http://127.0.0.1:1/");
        req.method = "GET\r\nX-Injected: 1".to_string();
        let err = perform(&req, &no_mock(), true).unwrap_err();
        assert!(matches!(err, CurlError::Connect(_)), "{err:?}");
    }

    #[test]
    fn connection_refused_maps_to_connect_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap_err();
        match &err {
            CurlError::Connect(detail) => assert!(detail.contains("failed to connect"), "{detail}"),
            other => panic!("expected Connect, got {other:?}"),
        }
        assert_eq!(err.exit_code(), 7);
    }

    #[test]
    fn error_messages_and_exit_codes_are_curl_shaped() {
        let cases = [
            (CurlError::NetworkDisabled, 7, "curl: (7) "),
            (
                CurlError::UnsupportedScheme("https".to_string()),
                1,
                "curl: (1) ",
            ),
            (CurlError::Connect("boom".to_string()), 7, "curl: (7) boom"),
            (CurlError::TooLarge { limit: 10 }, 63, "curl: (63) "),
            (CurlError::Timeout, 28, "curl: (28) "),
        ];
        for (err, code, prefix) in cases {
            assert_eq!(err.exit_code(), code);
            let message = err.message();
            assert!(message.starts_with(prefix), "{message}");
            assert!(!message.ends_with('\n'), "{message}");
        }
    }

    // ------------------------------------------------------- real loopback HTTP

    #[test]
    fn loopback_get_returns_status_and_body() {
        let (port, handle) = spawn_server(|_| {
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello"
                .to_vec()
        });
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.body, "hello");
        assert_eq!(
            response.headers,
            vec![
                ("Content-Length".to_string(), "5".to_string()),
                ("Content-Type".to_string(), "text/plain".to_string()),
            ]
        );
    }

    #[test]
    fn loopback_sends_request_line_and_host_header() {
        let (port, handle) = spawn_server(|received| {
            let head = request_head(received);
            let request_line = head.lines().next().unwrap_or("").to_string();
            let host = head
                .lines()
                .find_map(|line| line.strip_prefix("host:"))
                .unwrap_or("")
                .trim()
                .to_string();
            let body = format!("{request_line}|{host}");
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        });
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/path?q=1")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(
            response.body,
            format!("get /path?q=1 http/1.1|127.0.0.1:{port}")
        );
    }

    #[test]
    fn loopback_post_sends_body_and_content_length() {
        let (port, handle) = spawn_server(|received| {
            let text = String::from_utf8_lossy(received).to_string();
            let head = request_head(received);
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap_or("")
                .trim()
                .to_string();
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let payload = format!("{length}:{body}");
            format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            )
            .into_bytes()
        });
        let mut req = request("POST", &format!("http://127.0.0.1:{port}/submit"));
        req.body = Some("a=1&b=2".to_string());
        let response = perform(&req, &no_mock(), true).unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.reason, "Created");
        assert_eq!(response.body, "7:a=1&b=2");
    }

    #[test]
    fn loopback_echoes_custom_headers() {
        let (port, handle) = spawn_server(|received| {
            let head = request_head(received);
            let value = head
                .lines()
                .find_map(|line| line.strip_prefix("x-token:"))
                .unwrap_or("")
                .trim()
                .to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{value}",
                value.len()
            )
            .into_bytes()
        });
        let mut req = request("GET", &format!("http://127.0.0.1:{port}/"));
        req.headers = vec![
            ("X-Token".to_string(), "abc123".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let response = perform(&req, &no_mock(), true).unwrap();
        handle.join().unwrap();
        assert_eq!(response.body, "abc123");
    }

    #[test]
    fn loopback_decodes_chunked_response() {
        let (port, handle) = spawn_server(|_| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
                .to_vec()
        });
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello world");
    }

    #[test]
    fn loopback_decodes_chunked_response_with_extension_and_trailer() {
        let (port, handle) = spawn_server(|_| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              3;ext=1\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\n"
                .to_vec()
        });
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.body, "abc");
    }

    #[test]
    fn loopback_hostile_chunk_size_cannot_overflow() {
        // `usize::MAX` as a chunk size must be rejected, not overflow a size computation.
        let (port, handle) = spawn_server(|_| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n".to_vec()
        });
        let err = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap_err();
        handle.join().unwrap();
        assert_eq!(
            err,
            CurlError::TooLarge {
                limit: MAX_RESPONSE_BYTES
            }
        );
    }

    #[test]
    fn loopback_invalid_chunk_size_is_rejected() {
        let (port, handle) = spawn_server(|_| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n".to_vec()
        });
        let err = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap_err();
        handle.join().unwrap();
        assert!(matches!(err, CurlError::Connect(_)), "{err:?}");
    }

    #[test]
    fn loopback_malformed_status_line_is_rejected() {
        let (port, handle) = spawn_server(|_| b"NOT-HTTP\r\n\r\n".to_vec());
        let err = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap_err();
        handle.join().unwrap();
        assert!(matches!(err, CurlError::Connect(_)), "{err:?}");
    }

    #[test]
    fn loopback_binary_body_is_decoded_lossily() {
        let (port, handle) = spawn_server(|_| {
            let mut wire = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n".to_vec();
            wire.extend_from_slice(&[0xff, 0xfe, b'o', b'k']);
            wire
        });
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.body, "\u{fffd}\u{fffd}ok");
    }

    #[test]
    fn loopback_reads_to_eof_without_content_length() {
        let (port, handle) = spawn_server(|_| b"HTTP/1.1 200 OK\r\n\r\nraw-eof".to_vec());
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.body, "raw-eof");
    }

    #[test]
    fn loopback_reports_404_reason_phrase() {
        let (port, handle) = spawn_server(|_| {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".to_vec()
        });
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/missing")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 404);
        assert_eq!(response.reason, "Not Found");
        assert_eq!(response.body, "not found");
    }

    #[test]
    fn loopback_head_response_has_no_body() {
        let (port, handle) = spawn_server(|received| {
            let head = request_head(received);
            assert!(head.starts_with("head /"), "{head}");
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_vec()
        });
        let response = perform(
            &request("HEAD", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "");
    }

    #[test]
    fn loopback_204_has_no_body() {
        let (port, handle) = spawn_server(|_| b"HTTP/1.1 204 No Content\r\n\r\n".to_vec());
        let response = perform(
            &request("DELETE", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 204);
        assert_eq!(response.body, "");
    }

    #[test]
    fn loopback_http10_status_line_is_accepted() {
        let (port, handle) = spawn_server(|_| b"HTTP/1.0 200 OK\r\n\r\nlegacy".to_vec());
        let response = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/")),
            &no_mock(),
            true,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(response.body, "legacy");
    }

    #[test]
    fn loopback_oversize_response_is_capped() {
        let payload = "y".repeat(MAX_RESPONSE_BYTES + 4096);
        let (port, handle) = spawn_server(move |_| {
            let mut wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            )
            .into_bytes();
            wire.extend_from_slice(payload.as_bytes());
            wire
        });
        let err = perform(
            &request("GET", &format!("http://127.0.0.1:{port}/big")),
            &no_mock(),
            true,
        )
        .unwrap_err();
        handle.join().unwrap();
        assert_eq!(
            err,
            CurlError::TooLarge {
                limit: MAX_RESPONSE_BYTES
            }
        );
    }

    #[test]
    fn loopback_multiple_requests_are_independent() {
        for _ in 0..3 {
            let (port, handle) =
                spawn_server(|_| b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec());
            let response = perform(
                &request("GET", &format!("http://127.0.0.1:{port}/")),
                &no_mock(),
                true,
            )
            .unwrap();
            handle.join().unwrap();
            assert_eq!(response.body, "ok");
        }
    }

    #[test]
    fn documented_limits_are_enforced_consistently() {
        // The mock path and the socket path share the same cap, so a body of exactly the
        // limit is accepted and one byte more is not.
        let mut mock = no_mock();
        mock.insert(
            "http://example.com/big".to_string(),
            "z".repeat(MAX_RESPONSE_BYTES + 1),
        );
        let err = perform(&request("GET", "http://example.com/big"), &mock, false).unwrap_err();
        assert_eq!(err.exit_code(), 63);
        assert!(err.message().contains(&MAX_RESPONSE_BYTES.to_string()));
    }
}
