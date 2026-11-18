//! deploy/harness/drill.rs — standalone harness for the Cloud backup drill.
//!
//! Built with rustc directly against the previously built `muster` library
//! (no Cargo manifest or lock changes; see harness/build.sh). Uses only the
//! public library API the CLI itself runs (signing, cloud_protocol,
//! fleet_state::seal, manifest) plus plain HTTP — the same public seams as
//! the product: HTTP API, real Git bundles, real Ed25519 signatures. No
//! internal test code is linked.
//!
//! Subcommands:
//!   drill idp --port <0|port> --client-id ID --client-secret S
//!             --subject alice --email e --key-file path
//!     Fixture OIDC provider (RS256 via the openssl CLI). Prints
//!     "issuer=http://127.0.0.1:<port>" once listening, then serves.
//!   drill seed --endpoint URL --idp ISSUER --client-id ID --client-secret S
//!              --work-dir DIR --out STATE
//!     Signs in through the fixture IdP (browser stand-in), creates a
//!     workspace, runs the RFC 8628 device flow, links this machine
//!     (real link proof) and pushes a real signed genesis plus one
//!     incremental revision. Writes STATE (0600).
//!   drill verify --endpoint URL --state STATE --expect-head SHA
//!                --push yes|no --label L
//!     Public-API recovery checks against a (restored) service instance:
//!     /readyz, old session cookie on /v1/me, old bearer on
//!     /v1/workspaces, signed full pull (revision + bundle roundtrip),
//!     and optionally a signed push (server-side trust verification).
//!
//! Synthetic data only; tokens land in the state file (0600), never in
//! argv or logs.

//!   drill freeport
//!     Binds 127.0.0.1:0, prints the picked port, exits.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use muster::cloud_protocol::{
    body_sha256_hex, canonical_link_v1, canonical_request_v1, BASE_REVISION_NONE,
    DEVICE_CODE_GRANT_TYPE, HEADER_BASE_REVISION, HEADER_CONTENT_SHA256, HEADER_DEVICE_ID,
    HEADER_EPOCH, HEADER_IDEMPOTENCY_KEY, HEADER_NONCE, HEADER_REVISION, HEADER_SIGNATURE,
    HEADER_TIMESTAMP,
};
use muster::fleet_state::seal;
use muster::manifest::{Machine, Manifest};
use muster::signing::Signer;

// ---------------------------------------------------------------------------
// small utils
// ---------------------------------------------------------------------------

fn die(msg: &str) -> ! {
    eprintln!("drill: FAIL: {msg}");
    std::process::exit(1);
}

fn check(cond: bool, what: &str) {
    if !cond {
        die(what);
    }
    eprintln!("drill: ok: {what}");
}

fn urandom(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let mut f = std::fs::File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(&mut buf).expect("read /dev/urandom");
    buf
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

const B64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const B64STD: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_alphabet(alphabet: &[u8], bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    // No-padding alphabets must not emit the trailing '=' fillers.
    if alphabet.len() == 64 && alphabet[62] == b'-' {
        return out.trim_end_matches('=').to_string();
    }
    out
}

fn b64url(bytes: &[u8]) -> String {
    b64_alphabet(B64URL, bytes)
}

fn b64std(bytes: &[u8]) -> String {
    b64_alphabet(B64STD, bytes)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

fn uuid_v4() -> String {
    let b = urandom(16);
    let mut b = b;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Value of `"key":"…"` in a JSON body (values are simple: no embedded
/// quotes — tokens, uuids, urls, emails in this drill).
fn json_str(body: &str, key: &str) -> String {
    let pat = format!("\"{key}\":");
    let mut from = 0;
    while let Some(pos) = body[from..].find(&pat) {
        let at = from + pos + pat.len();
        let rest = body[at..].trim_start();
        if let Some(rest) = rest.strip_prefix('"') {
            if let Some(end) = rest.find('"') {
                return rest[..end].to_string();
            }
        }
        from = at;
    }
    String::new()
}

fn json_contains(body: &str, needle: &str) -> bool {
    body.contains(needle)
}

// ---------------------------------------------------------------------------
// minimal HTTP client (http:// only, Connection: close)
// ---------------------------------------------------------------------------

struct Resp {
    status: u16,
    headers: Vec<(String, String)>, // lowercased names
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<String> {
        let lname = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == lname)
            .map(|(_, v)| v.clone())
    }
    /// First Set-Cookie as `name=value` (attributes stripped).
    fn cookie(&self, name: &str) -> Option<String> {
        let lname = "set-cookie";
        for (n, v) in &self.headers {
            if n == lname {
                let first = v.split(';').next().unwrap_or("").trim();
                if let Some(value) = first.strip_prefix(&format!("{name}=")) {
                    return Some(format!("{name}={value}"));
                }
            }
        }
        None
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn http_req(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<Resp> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("only http:// URLs"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().expect("port")),
        None => (authority.to_string(), 80),
    };
    let mut stream = TcpStream::connect((host.as_str(), port))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let sep =
        find(&raw, b"\r\n\r\n").ok_or_else(|| std::io::Error::other("no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..sep]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let mut resp_headers = Vec::new();
    for line in lines {
        if let Some((n, v)) = line.split_once(':') {
            resp_headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let mut resp_body = raw[sep + 4..].to_vec();
    let chunked = resp_headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        resp_body = de_chunk(&resp_body);
    }
    Ok(Resp {
        status,
        headers: resp_headers,
        body: resp_body,
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Application/x-www-form-urlencoded decoding: '+' is a space, %XX hex.
fn url_decode(s: &str) -> String {
    let plus_fixed = s.replace('+', " ");
    let mut out = Vec::new();
    let bytes = plus_fixed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hexs = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz");
            if let Ok(v) = u8::from_str_radix(hexs, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn de_chunk(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find(&data[i..], b"\r\n") {
        let line_end = i + p;
        let size_str = String::from_utf8_lossy(&data[i..line_end]);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(data.len());
        out.extend_from_slice(&data[start..end]);
        i = end + 2;
    }
    out
}

// ---------------------------------------------------------------------------
// fixture OIDC IdP (RS256 through the openssl CLI)
// ---------------------------------------------------------------------------

struct Pending {
    redirect_uri: String,
    challenge: String,
    nonce: String,
    subject: String,
    email: String,
}

fn openssl(args: &[&str]) -> Vec<u8> {
    let out = Command::new("openssl")
        .args(args)
        .output()
        .expect("openssl");
    if !out.status.success() {
        die(&format!(
            "openssl {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    out.stdout
}

fn jwk_n(key_file: &Path) -> String {
    let text = String::from_utf8_lossy(&openssl(&[
        "rsa",
        "-in",
        key_file.to_str().unwrap(),
        "-noout",
        "-modulus",
    ]))
    .into_owned();
    let hexmod = text
        .lines()
        .find_map(|l| l.strip_prefix("Modulus="))
        .unwrap_or_else(|| die("cannot parse modulus"))
        .trim()
        .to_string();
    let mut bytes = unhex(&hexmod);
    while bytes.len() > 1 && bytes[0] == 0 {
        bytes.remove(0);
    }
    b64url(&bytes)
}

fn cmd_idp(args: &[String]) {
    let mut port = 0u16;
    let mut client_id = String::new();
    let mut client_secret = String::new();
    let mut subject = String::new();
    let mut email = String::new();
    let mut key_file = PathBuf::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                port = args[i + 1].parse().expect("port");
                i += 2
            }
            "--client-id" => {
                client_id = args[i + 1].clone();
                i += 2
            }
            "--client-secret" => {
                client_secret = args[i + 1].clone();
                i += 2
            }
            "--subject" => {
                subject = args[i + 1].clone();
                i += 2
            }
            "--email" => {
                email = args[i + 1].clone();
                i += 2
            }
            "--key-file" => {
                key_file = PathBuf::from(&args[i + 1]);
                i += 2
            }
            other => die(&format!("idp: unknown arg {other}")),
        }
    }
    check(!client_id.is_empty(), "idp: client id set");
    check(!email.is_empty(), "idp: email set");
    if !key_file.exists() {
        openssl(&[
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
            "-out",
            key_file.to_str().unwrap(),
        ]);
    }
    let n = jwk_n(&key_file);

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("idp bind");
    let issuer = format!("http://{}", listener.local_addr().expect("idp addr"));
    println!("issuer={issuer}");
    let codes: Arc<Mutex<HashMap<String, Pending>>> = Arc::new(Mutex::new(HashMap::new()));

    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(c) => c,
            Err(_) => continue,
        };
        let codes = Arc::clone(&codes);
        let n = n.clone();
        let issuer = issuer.clone();
        let (client_id, client_secret, subject, email) = (
            client_id.clone(),
            client_secret.clone(),
            subject.clone(),
            email.clone(),
        );
        let key_file = key_file.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // Read headers, then keep reading until a short timeout: tiny
            // POST bodies may arrive in a separate packet.
            let mut saw_headers = false;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => {
                        raw.extend_from_slice(&buf[..k]);
                        if !saw_headers && find(&raw, b"\r\n\r\n").is_some() {
                            saw_headers = true;
                            let _ = stream
                                .set_read_timeout(Some(std::time::Duration::from_millis(250)));
                        } else if saw_headers {
                            break;
                        }
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let mut parts = text.splitn(2, "\r\n\r\n");
            let head = parts.next().unwrap_or_default().to_string();
            let body = parts.next().unwrap_or_default().to_string();
            let mut lines = head.split("\r\n");
            let req_line = lines.next().unwrap_or_default().to_string();
            let mut req_headers = Vec::new();
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    req_headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
                }
            }
            let mut req_parts = req_line.split_whitespace();
            let method = req_parts.next().unwrap_or_default().to_string();
            let target = req_parts.next().unwrap_or_default().to_string();
            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p.to_string(), q.to_string()),
                None => (target.clone(), String::new()),
            };
            let params = |q: &str| -> HashMap<String, String> {
                q.split('&')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        let (k, v) = p.split_once('=').unwrap_or((p, ""));
                        (url_decode(k), url_decode(v))
                    })
                    .collect()
            };

            let mut respond = |status: &str, ctype: &str, location: Option<&str>, body: &[u8]| {
                let mut resp = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                if let Some(loc) = location {
                    resp.push_str(&format!("Location: {loc}\r\n"));
                }
                resp.push_str("\r\n");
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body);
            };

            match (method.as_str(), path.as_str()) {
                ("GET", "/.well-known/openid-configuration") => {
                    let body = format!(
                        "{{\"issuer\":\"{issuer}\",\"authorization_endpoint\":\"{issuer}/authorize\",\"token_endpoint\":\"{issuer}/token\",\"jwks_uri\":\"{issuer}/jwks\",\"response_types_supported\":[\"code\"],\"subject_types_supported\":[\"public\"],\"id_token_signing_alg_values_supported\":[\"RS256\"],\"scopes_supported\":[\"openid\",\"email\",\"profile\"],\"token_endpoint_auth_methods_supported\":[\"client_secret_basic\"],\"code_challenge_methods_supported\":[\"S256\"]}}"
                    );
                    respond("200 OK", "application/json", None, body.as_bytes());
                }
                ("GET", "/jwks") => {
                    let body = format!(
                        "{{\"keys\":[{{\"kty\":\"RSA\",\"use\":\"sig\",\"alg\":\"RS256\",\"kid\":\"test-key-1\",\"n\":\"{n}\",\"e\":\"AQAB\"}}]}}"
                    );
                    respond("200 OK", "application/json", None, body.as_bytes());
                }
                ("GET", "/authorize") => {
                    let q = params(&query);
                    let mut bad = |msg: &str| {
                        respond(
                            "400 Bad Request",
                            "application/json",
                            None,
                            format!(
                                "{{\"error\":\"invalid_request\",\"error_description\":\"{msg}\"}}"
                            )
                            .as_bytes(),
                        )
                    };
                    if q.get("response_type").map(String::as_str) != Some("code") {
                        bad("response_type must be code");
                        return;
                    }
                    if q.get("client_id").map(String::as_str) != Some(client_id.as_str()) {
                        bad("unknown client_id");
                        return;
                    }
                    if q.get("code_challenge_method").map(String::as_str) != Some("S256") {
                        bad("PKCE S256 required");
                        return;
                    }
                    if !q
                        .get("scope")
                        .map(|s| s.split(' ').any(|x| x == "openid"))
                        .unwrap_or(false)
                    {
                        bad("openid scope required");
                        return;
                    }
                    let (Some(redirect_uri), Some(state), Some(nonce), Some(challenge)) = (
                        q.get("redirect_uri").cloned(),
                        q.get("state").cloned(),
                        q.get("nonce").cloned(),
                        q.get("code_challenge").cloned(),
                    ) else {
                        bad("redirect_uri, state, nonce and code_challenge are required");
                        return;
                    };
                    let Some(login_as) = q.get("login_as").cloned() else {
                        bad("fixture needs login_as=<subject>");
                        return;
                    };
                    if login_as != subject {
                        bad("unknown fixture subject");
                        return;
                    }
                    let code = hex(&urandom(16));
                    codes.lock().unwrap().insert(
                        code.clone(),
                        Pending {
                            redirect_uri: redirect_uri.clone(),
                            challenge,
                            nonce,
                            subject: login_as,
                            email: email.clone(),
                        },
                    );
                    let location = format!("{redirect_uri}?code={code}&state={state}");
                    respond("303 See Other", "text/plain", Some(&location), b"");
                }
                ("POST", "/token") => {
                    let mut bad = |err: &str| {
                        respond(
                            "400 Bad Request",
                            "application/json",
                            None,
                            format!("{{\"error\":\"{err}\"}}").as_bytes(),
                        )
                    };
                    let expected = format!(
                        "Basic {}",
                        b64std(format!("{client_id}:{client_secret}").as_bytes())
                    );
                    let authorized = req_headers
                        .iter()
                        .any(|(name, value)| name == "authorization" && value == &expected);
                    if !authorized {
                        respond(
                            "401 Unauthorized",
                            "application/json",
                            None,
                            b"{\"error\":\"invalid_client\"}",
                        );
                        return;
                    }
                    let form = params(&body);
                    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
                        bad("invalid_grant");
                        return;
                    }
                    let Some(code) = form.get("code").cloned() else {
                        bad("invalid_grant");
                        return;
                    };
                    let Some(pending) = codes.lock().unwrap().remove(&code) else {
                        bad("invalid_grant");
                        return;
                    };
                    if form.get("redirect_uri").map(|s| s.as_str())
                        != Some(pending.redirect_uri.as_str())
                    {
                        bad("invalid_grant");
                        return;
                    }
                    let Some(verifier) = form.get("code_verifier").cloned() else {
                        bad("invalid_grant");
                        return;
                    };
                    let computed = b64url(&unhex(&body_sha256_hex(verifier.as_bytes())));
                    if computed != pending.challenge {
                        bad("invalid_grant");
                        return;
                    }
                    let now = now_secs();
                    let header = b64url(br#"{"typ":"JWT","alg":"RS256","kid":"test-key-1"}"#);
                    let payload = b64url(
                        format!(
                            "{{\"iss\":\"{issuer}\",\"sub\":\"{}\",\"aud\":\"{client_id}\",\"exp\":{},\"iat\":{},\"nonce\":\"{}\",\"email\":\"{}\",\"email_verified\":true}}",
                            pending.subject,
                            now + 600,
                            now,
                            pending.nonce,
                            pending.email
                        )
                        .as_bytes(),
                    );
                    let signing_input = format!("{header}.{payload}");
                    let input_file =
                        std::env::temp_dir().join(format!("drill-jwt-{}.in", std::process::id()));
                    let sig_file =
                        std::env::temp_dir().join(format!("drill-jwt-{}.sig", std::process::id()));
                    std::fs::write(&input_file, signing_input.as_bytes()).expect("write jwt input");
                    openssl(&[
                        "dgst",
                        "-sha256",
                        "-sign",
                        key_file.to_str().unwrap(),
                        "-out",
                        sig_file.to_str().unwrap(),
                        input_file.to_str().unwrap(),
                    ]);
                    let sig = std::fs::read(&sig_file).expect("read jwt sig");
                    let _ = std::fs::remove_file(&input_file);
                    let _ = std::fs::remove_file(&sig_file);
                    let id_token = format!("{signing_input}.{}", b64url(&sig));
                    let body = format!(
                        "{{\"access_token\":\"idp-opaque-token\",\"token_type\":\"Bearer\",\"expires_in\":3600,\"id_token\":\"{id_token}\",\"scope\":\"openid email\"}}"
                    );
                    respond("200 OK", "application/json", None, body.as_bytes());
                }
                _ => respond("404 Not Found", "text/plain", None, b"not found"),
            }
        });
    }
}

// ---------------------------------------------------------------------------
// browser stand-in: OIDC login through the fixture IdP
// ---------------------------------------------------------------------------

fn login(
    endpoint: &str,
    client_id: &str,
    client_secret: &str,
    subject: &str,
    email: &str,
) -> (String, String, String) {
    let resp = http_req(
        "GET",
        &format!("{endpoint}/auth/login?return_to=/"),
        &[],
        b"",
    )
    .unwrap_or_else(|e| die(&format!("GET /auth/login: {e}")));
    check(resp.status == 303, "login: /auth/login redirects");
    let login_cookie = resp
        .cookie("__Host-mc_login")
        .unwrap_or_else(|| die("login: no __Host-mc_login cookie"));
    let authorize_url = resp
        .header("Location")
        .unwrap_or_else(|| die("login: no Location from /auth/login"));

    // Fixture contract: the browser signs in by appending login_as/email.
    let sep = if authorize_url.contains('?') {
        '&'
    } else {
        '?'
    };
    let authorize_url = format!("{authorize_url}{sep}login_as={subject}&email={email}");
    let resp = http_req("GET", &authorize_url, &[], b"")
        .unwrap_or_else(|e| die(&format!("GET authorize: {e}")));
    if resp.status != 303 {
        die(&format!(
            "login: IdP authorize did not redirect: status={} body={}",
            resp.status,
            resp.text()
        ));
    }
    let callback_url = resp
        .header("Location")
        .unwrap_or_else(|| die("login: no Location from IdP"));
    let resp = http_req("GET", &callback_url, &[("Cookie", login_cookie)], b"")
        .unwrap_or_else(|e| die(&format!("GET callback: {e}")));
    if resp.status != 303 {
        die(&format!(
            "login: callback did not redirect: status={} body={} url={}",
            resp.status,
            resp.text(),
            callback_url
        ));
    }
    let session_cookie = resp
        .cookie("__Host-mc_session")
        .unwrap_or_else(|| die("login: no __Host-mc_session cookie"));

    let resp = http_req(
        "GET",
        &format!("{endpoint}/v1/me"),
        &[("Cookie", session_cookie.clone())],
        b"",
    )
    .unwrap_or_else(|e| die(&format!("GET /v1/me: {e}")));
    check(resp.status == 200, "login: /v1/me with session");
    let me = resp.text();
    let account_id = json_str(&me, "id");
    let csrf = json_str(&me, "csrf_token");
    check(!account_id.is_empty(), "login: account id");
    check(!csrf.is_empty(), "login: csrf token");
    let _ = (client_id, client_secret);
    (session_cookie, account_id, csrf)
}

// ---------------------------------------------------------------------------
// signed device requests (protocol v1, via the real library)
// ---------------------------------------------------------------------------

struct Device {
    endpoint: String,
    workspace: String,
    id: String,
    token: String,
    signer: Signer,
}

impl Device {
    fn signed(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        base: Option<&str>,
        idem: Option<&str>,
    ) -> Resp {
        let nonce = format!("n{}", hex(&urandom(16)));
        let ts = now_secs().to_string();
        let sha = body_sha256_hex(body);
        let bytes = canonical_request_v1(&muster::cloud_protocol::SignedRequestV1 {
            method,
            path_and_query,
            workspace_id: &self.workspace,
            device_id: &self.id,
            body_sha256_hex: &sha,
            base_revision: base.unwrap_or(""),
            idempotency_key: idem.unwrap_or(""),
            timestamp: &ts,
            nonce: &nonce,
        });
        let signature = format!("v1={}", self.signer.sign(&bytes));
        let mut headers: Vec<(&str, String)> = vec![
            ("Authorization", format!("Bearer {}", self.token)),
            (HEADER_DEVICE_ID, self.id.clone()),
            (HEADER_TIMESTAMP, ts),
            (HEADER_NONCE, nonce),
            (HEADER_CONTENT_SHA256, sha),
            (HEADER_SIGNATURE, signature),
        ];
        if let Some(b) = base {
            headers.push((HEADER_BASE_REVISION, b.to_string()));
        }
        if let Some(k) = idem {
            headers.push((HEADER_IDEMPOTENCY_KEY, k.to_string()));
        }
        http_req(
            method,
            &format!("{}{}", self.endpoint, path_and_query),
            &headers,
            body,
        )
        .unwrap_or_else(|e| die(&format!("{method} {path_and_query}: {e}")))
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    if !out.status.success() {
        die(&format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// seed
// ---------------------------------------------------------------------------

fn cmd_seed(args: &[String]) {
    let mut endpoint = String::new();
    let mut idp = String::new();
    let mut work = PathBuf::new();
    let mut out = PathBuf::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--endpoint" => {
                endpoint = args[i + 1].clone();
                i += 2
            }
            "--idp" => {
                idp = args[i + 1].clone();
                i += 2
            }
            "--work-dir" => {
                work = PathBuf::from(&args[i + 1]);
                i += 2
            }
            "--out" => {
                out = PathBuf::from(&args[i + 1]);
                i += 2
            }
            other => die(&format!("seed: unknown arg {other}")),
        }
    }
    check(
        !endpoint.is_empty() && !idp.is_empty(),
        "seed: endpoint and idp set",
    );

    // 1) Browser-style login through the fixture IdP.
    let (session_cookie, account_id, csrf) = login(
        &endpoint,
        "muster-drill",
        "drill-client-secret-not-real",
        "alice",
        "alice@drill.invalid",
    );

    // 2) Create a workspace (session + CSRF + Origin + Idempotency-Key).
    let resp = http_req(
        "POST",
        &format!("{endpoint}/v1/workspaces"),
        &[
            ("Cookie", session_cookie.clone()),
            ("Origin", endpoint.clone()),
            ("X-CSRF-Token", csrf.clone()),
            ("Idempotency-Key", uuid_v4()),
            ("Content-Type", "application/json".into()),
        ],
        br#"{"name":"Drill WS"}"#,
    )
    .unwrap_or_else(|e| die(&format!("POST /v1/workspaces: {e}")));
    check(resp.status == 201, "seed: workspace created (201)");
    let ws = json_str(&resp.text(), "id");
    check(!ws.is_empty(), "seed: workspace id");

    // 3) Device flow: authorize -> approve (browser session) -> token.
    let resp = http_req(
        "POST",
        &format!("{endpoint}/v1/device/authorize"),
        &[("Content-Type", "application/json".into())],
        br#"{"client_name":"drill"}"#,
    )
    .unwrap_or_else(|e| die(&format!("POST /v1/device/authorize: {e}")));
    check(resp.status == 200, "seed: device authorize");
    let auth = resp.text();
    let device_code = json_str(&auth, "device_code");
    let user_code = json_str(&auth, "user_code");
    check(!device_code.is_empty(), "seed: device code");
    let resp = http_req(
        "POST",
        &format!("{endpoint}/v1/device/approve"),
        &[
            ("Cookie", session_cookie.clone()),
            ("Origin", endpoint.clone()),
            ("X-CSRF-Token", csrf.clone()),
            ("Content-Type", "application/json".into()),
        ],
        format!("{{\"user_code\":\"{user_code}\",\"decision\":\"approve\"}}").as_bytes(),
    )
    .unwrap_or_else(|e| die(&format!("POST /v1/device/approve: {e}")));
    check(resp.status == 200, "seed: device approved");
    let resp = http_req(
        "POST",
        &format!("{endpoint}/v1/device/token"),
        &[("Content-Type", "application/json".into())],
        format!(
            "{{\"grant_type\":\"{DEVICE_CODE_GRANT_TYPE}\",\"device_code\":\"{device_code}\"}}"
        )
        .as_bytes(),
    )
    .unwrap_or_else(|e| die(&format!("POST /v1/device/token: {e}")));
    check(resp.status == 200, "seed: account token issued");
    let bearer = json_str(&resp.text(), "access_token");
    check(bearer.starts_with("mca_"), "seed: bearer token shape");

    // 4) Real signed genesis fleet state (library the CLI runs).
    let machine_state = work.join("machine-state");
    let config = work.join("fleet-config");
    let state_dir = work.join("fleet-state");
    std::fs::create_dir_all(config.join("vault")).expect("fleet config dir");
    std::fs::create_dir_all(&state_dir).expect("fleet state dir");
    git(&config, &["init", "-q", "-b", "main"]);
    git(&config, &["config", "user.name", "drill"]);
    git(&config, &["config", "user.email", "d@drill.invalid"]);
    let signer = Signer::load_or_create(&machine_state).expect("signer");
    let signing_key = signer.public();
    let recipient = format!("age1{}synthetic", hex(&urandom(8)));
    let machine = "drill-a".to_string();
    let mut manifest = Manifest::new();
    manifest.machines.insert(
        machine.clone(),
        Machine {
            recipient: recipient.clone(),
            signing_key: Some(signing_key.clone()),
            platform: "linux-x86_64".into(),
            muster_version: "drill".into(),
            agents: vec![],
            last_sync: None,
            host: None,
            role: None,
            profile: None,
            applied_profile: None,
        },
    );
    std::fs::write(config.join("vault/default.age"), b"synthetic-ciphertext")
        .expect("vault fixture");
    manifest.save(&config).expect("manifest save");
    seal(&config, &state_dir, &manifest, &signer, true)
        .expect("seal")
        .expect("sealed genesis");
    git(&config, &["add", "-A"]);
    git(
        &config,
        &[
            "-c",
            "user.name=drill",
            "-c",
            "user.email=d@drill.invalid",
            "commit",
            "-q",
            "-m",
            "genesis",
        ],
    );
    let head1 = git(&config, &["rev-parse", "HEAD"]);
    let bundle_file = work.join("genesis.bundle");
    git(
        &config,
        &[
            "bundle",
            "create",
            "-q",
            bundle_file.to_str().unwrap(),
            "main",
        ],
    );
    let bundle = std::fs::read(&bundle_file).expect("genesis bundle");

    // 5) Link this machine (real proof over canonical_link_v1).
    let proof = signer.sign(&canonical_link_v1(
        &ws,
        &account_id,
        &machine,
        &signing_key,
        &recipient,
    ));
    let resp = http_req(
        "POST",
        &format!("{endpoint}/v1/workspaces/{ws}/devices"),
        &[
            ("Authorization", format!("Bearer {bearer}")),
            ("Idempotency-Key", uuid_v4()),
            ("Content-Type", "application/json".into()),
        ],
        format!(
            "{{\"machine_name\":\"{machine}\",\"signing_key\":\"{signing_key}\",\"recipient\":\"{recipient}\",\"proof\":\"{proof}\"}}"
        )
        .as_bytes(),
    )
    .unwrap_or_else(|e| die(&format!("POST devices: {e}")));
    check(resp.status == 201, "seed: device linked (201)");
    let linked = resp.text();
    let device_id = json_str(&linked, "device_id");
    let device_token = json_str(&linked, "device_token");
    check(
        !device_id.is_empty() && !device_token.is_empty(),
        "seed: device credentials",
    );

    // 6) Push genesis (signed request, CAS base "none").
    let device = Device {
        endpoint: endpoint.clone(),
        workspace: ws.clone(),
        id: device_id.clone(),
        token: device_token.clone(),
        signer,
    };
    let resp = device.signed(
        "POST",
        &format!("/v1/workspaces/{ws}/sync"),
        &bundle,
        Some(BASE_REVISION_NONE),
        Some(&uuid_v4()),
    );
    check(resp.status == 200, "seed: genesis accepted (200)");
    let served1 = resp.header(HEADER_REVISION).unwrap_or_default();
    check(served1 == head1, "seed: genesis revision echoed");

    // 7) Ordinary change (no new epoch) + incremental push.
    std::fs::write(config.join("drill-note.txt"), b"synthetic drill note\n").expect("note");
    git(&config, &["add", "-A"]);
    git(
        &config,
        &[
            "-c",
            "user.name=drill",
            "-c",
            "user.email=d@drill.invalid",
            "commit",
            "-q",
            "-m",
            "drill note",
        ],
    );
    let head2 = git(&config, &["rev-parse", "HEAD"]);
    let inc_file = work.join("incremental.bundle");
    git(
        &config,
        &[
            "bundle",
            "create",
            "-q",
            inc_file.to_str().unwrap(),
            &format!("{head1}..main"),
        ],
    );
    let inc = std::fs::read(&inc_file).expect("incremental bundle");
    let resp = device.signed(
        "POST",
        &format!("/v1/workspaces/{ws}/sync"),
        &inc,
        Some(&head1),
        Some(&uuid_v4()),
    );
    check(resp.status == 200, "seed: incremental accepted (200)");
    let served2 = resp.header(HEADER_REVISION).unwrap_or_default();
    check(served2 == head2, "seed: incremental revision echoed");

    // State for verify (0600; synthetic tokens only).
    let body = format!(
        "endpoint={endpoint}\nidp={idp}\nsession_cookie={session_cookie}\naccount_id={account_id}\ncsrf={csrf}\nbearer={bearer}\nws={ws}\ndevice_id={device_id}\ndevice_token={device_token}\nmachine={machine}\nsigning_key={signing_key}\nfleet_config={}\nhead1={head1}\nhead2={head2}\n",
        config.display()
    );
    std::fs::write(&out, body).expect("write state");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600))
            .expect("chmod state");
    }
    println!("seeded ws={ws} head1={head1} head2={head2}");
}

// ---------------------------------------------------------------------------
// verify (against a restored or original instance)
// ---------------------------------------------------------------------------

fn cmd_verify(args: &[String]) {
    let mut endpoint = String::new();
    let mut state = PathBuf::new();
    let mut expect_head = String::new();
    let mut push = false;
    let mut label = String::from("verify");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--endpoint" => {
                endpoint = args[i + 1].clone();
                i += 2
            }
            "--state" => {
                state = PathBuf::from(&args[i + 1]);
                i += 2
            }
            "--expect-head" => {
                expect_head = args[i + 1].clone();
                i += 2
            }
            "--push" => {
                push = args[i + 1] == "yes";
                i += 2
            }
            "--label" => {
                label = args[i + 1].clone();
                i += 2
            }
            other => die(&format!("verify: unknown arg {other}")),
        }
    }
    let text = std::fs::read_to_string(&state).unwrap_or_else(|e| die(&format!("read state: {e}")));
    let get = |key: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap_or_default()
            .to_string()
    };
    let session_cookie = get("session_cookie");
    let account_id = get("account_id");
    let bearer = get("bearer");
    let ws = get("ws");
    let device_id = get("device_id");
    let device_token = get("device_token");
    let config = PathBuf::from(get("fleet_config"));
    check(!ws.is_empty(), "verify: state loaded");

    // a) readiness of the instance under test.
    let resp = http_req("GET", &format!("{endpoint}/readyz"), &[], b"")
        .unwrap_or_else(|e| die(&format!("GET /readyz: {e}")));
    check(resp.status == 200, "verify: /readyz ready");

    // b) the pre-backup browser session still authenticates.
    let resp = http_req(
        "GET",
        &format!("{endpoint}/v1/me"),
        &[("Cookie", session_cookie)],
        b"",
    )
    .unwrap_or_else(|e| die(&format!("GET /v1/me: {e}")));
    check(
        resp.status == 200,
        "verify: pre-backup session valid on /v1/me",
    );
    check(
        json_str(&resp.text(), "id") == account_id,
        "verify: same account id",
    );

    // c) the pre-backup account token still lists the workspace.
    let resp = http_req(
        "GET",
        &format!("{endpoint}/v1/workspaces"),
        &[("Authorization", format!("Bearer {bearer}"))],
        b"",
    )
    .unwrap_or_else(|e| die(&format!("GET /v1/workspaces: {e}")));
    check(resp.status == 200, "verify: pre-backup bearer valid");
    check(json_contains(&resp.text(), &ws), "verify: workspace listed");

    // d) signed full pull: PG-referenced head served, bundle round-trips.
    let device = Device {
        endpoint: endpoint.clone(),
        workspace: ws.clone(),
        id: device_id,
        token: device_token,
        signer: Signer::load(&config.parent().unwrap().join("machine-state"))
            .ok()
            .flatten()
            .unwrap_or_else(|| die("verify: machine signer missing")),
    };
    let resp = device.signed("GET", &format!("/v1/workspaces/{ws}/sync"), b"", None, None);
    check(resp.status == 200, "verify: full pull 200");
    let served = resp.header(HEADER_REVISION).unwrap_or_default();
    check(
        served == expect_head,
        &format!("verify: served revision matches pre-backup head ({label})"),
    );
    let clone_dir = config.parent().unwrap().join(format!("clone-{label}"));
    let _ = std::fs::remove_dir_all(&clone_dir);
    std::fs::create_dir_all(&clone_dir).expect("clone dir");
    let bundle_file = clone_dir.join("pulled.bundle");
    std::fs::write(&bundle_file, &resp.body).expect("write pulled bundle");
    git(&clone_dir, &["init", "-q", "-b", "main"]);
    git(
        &clone_dir,
        &[
            "fetch",
            "-q",
            bundle_file.to_str().unwrap(),
            "refs/heads/main:refs/heads/pulled",
        ],
    );
    let pulled = git(&clone_dir, &["rev-parse", "pulled"]);
    check(pulled == expect_head, "verify: pulled bundle head matches");

    // e) optional signed push: server-side trust verification against the
    // restored anchor, CAS base at the pulled head.
    if push {
        std::fs::write(
            config.join(format!("verify-{label}.txt")),
            format!("synthetic verify marker {label}\n"),
        )
        .expect("marker");
        git(&config, &["add", "-A"]);
        git(
            &config,
            &[
                "-c",
                "user.name=drill",
                "-c",
                "user.email=d@drill.invalid",
                "commit",
                "-q",
                "-m",
                &format!("verify {label}"),
            ],
        );
        let new_head = git(&config, &["rev-parse", "HEAD"]);
        let inc = clone_dir.join("verify.bundle");
        git(
            &config,
            &[
                "bundle",
                "create",
                "-q",
                inc.to_str().unwrap(),
                &format!("{expect_head}..main"),
            ],
        );
        let body = std::fs::read(&inc).expect("verify bundle");
        let resp = device.signed(
            "POST",
            &format!("/v1/workspaces/{ws}/sync"),
            &body,
            Some(&expect_head),
            Some(&uuid_v4()),
        );
        check(
            resp.status == 200,
            "verify: incremental push accepted after restore",
        );
        check(
            resp.header(HEADER_REVISION).unwrap_or_default() == new_head,
            "verify: push revision echoed",
        );
        check(
            resp.header(HEADER_EPOCH)
                .unwrap_or_default()
                .parse::<u64>()
                .unwrap_or(0)
                >= 1,
            "verify: epoch header sane",
        );
        println!("verified label={label} new_head={new_head}");
    } else {
        println!("verified label={label} head={expect_head}");
    }
}

fn cmd_freeport() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("freeport bind");
    println!("{}", listener.local_addr().expect("freeport addr").port());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => die("usage: drill idp|seed|verify|freeport ..."),
    };
    match cmd {
        "idp" => cmd_idp(&rest),
        "seed" => cmd_seed(&rest),
        "verify" => cmd_verify(&rest),
        "freeport" => cmd_freeport(),
        other => die(&format!("unknown subcommand {other}")),
    }
}
