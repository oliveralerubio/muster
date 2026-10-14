//! The web admin interface behind `muster ui`.
//!
//! - Listens on 127.0.0.1 only. Startup prints `http://127.0.0.1:<port>/?token=<t>`;
//!   the one-time token is consumed by the first request that presents it and
//!   exchanged for an HttpOnly, SameSite=Strict session cookie. Everything else
//!   without a valid session answers 401.
//! - Every asset is embedded in the binary; the CSP allows only this origin.
//! - The API is the registry: `GET|POST /api/<command>` (dots or slashes,
//!   e.g. `/api/member/approve`) runs the same handler as the CLI and returns
//!   its envelope plus `cli`, the equivalent command line. Mutating commands
//!   need POST; confirmations need `{"confirm": true}` (the CLI's `--yes`).
//! - `POST /api/secret/reveal {"name", "confirm": true}` is the only path that
//!   returns a secret value, and it is recorded in the local history first.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::context::{Context, Globals};
use crate::errors::MusterError;
use crate::output::{Envelope, Format};
use crate::registry::{self, CommandSpec, ParamKind, Params};

const INDEX_HTML: &str = include_str!("assets/index.html");
const STYLE_CSS: &str = include_str!("assets/style.css");
const APP_JS: &str = include_str!("assets/app.js");

const COOKIE: &str = "muster_session";
const MAX_BODY: u64 = 64 * 1024;
/// Commands the web API never runs: they read secrets from stdin, start
/// servers, bootstrap identities, or reveal values outside the audited path.
const BLOCKED: &[&str] = &["ui", "init", "secret.add", "secret.rotate", "secret.get"];
/// Body keys that map to global flags rather than command parameters.
const CONFIRM: &str = "confirm";
const DRY_RUN: &str = "dry_run";

/// Serve until SIGINT (or end of input on an interactive stdin).
pub fn serve(ctx: &Context, port: u16, open: bool) -> Result<(), MusterError> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| MusterError::Operational(format!("cannot listen on 127.0.0.1:{port}: {e}")))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|addr| addr.port())
        .ok_or_else(|| MusterError::Operational("server has no TCP address".into()))?;
    let token = random_hex()?;
    let url = format!("http://127.0.0.1:{port}/?token={token}");

    let line = match ctx.format {
        Format::Json => json!({ "url": url, "port": port, "token": token }).to_string(),
        Format::Text => url.clone(),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
    drop(stdout);
    ctx.progress("Muster UI is running; press Ctrl-C to stop.");
    if open {
        open_browser(&url);
    }

    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    if std::io::stdin().is_terminal() {
        // Ctrl-D ends the session like Ctrl-C does.
        let (server, stop) = (Arc::clone(&server), Arc::clone(&stop));
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            stop.store(true, Ordering::SeqCst);
            server.unblock();
        });
    }

    let mut ui = Ui {
        config_dir: ctx.config_dir.clone(),
        state_dir: ctx.state_dir.clone(),
        port,
        token: Some(token),
        sessions: Vec::new(),
    };
    while !stop.load(Ordering::SeqCst) {
        match server.recv() {
            Ok(request) => ui.handle(request),
            Err(_) => break,
        }
    }
    Ok(())
}

struct Ui {
    config_dir: PathBuf,
    state_dir: PathBuf,
    port: u16,
    /// The one-time login token; `None` once used.
    token: Option<String>,
    sessions: Vec<String>,
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    cookie: Option<String>,
}

impl Reply {
    fn json(status: u16, value: Value) -> Self {
        Self::new(status, "application/json", value.to_string())
    }

    fn new(status: u16, content_type: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            cookie: None,
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, json!({ "ok": false, "error": message }))
    }
}

impl Ui {
    fn handle(&mut self, mut request: Request) {
        let reply = self.reply(&mut request);
        let mut response = Response::from_string(reply.body)
            .with_status_code(reply.status)
            .with_header(header("Content-Type", reply.content_type))
            .with_header(header("Cache-Control", "no-store"))
            .with_header(header("X-Content-Type-Options", "nosniff"))
            .with_header(header("X-Frame-Options", "DENY"))
            .with_header(header("Referrer-Policy", "no-referrer"))
            .with_header(header(
                "Content-Security-Policy",
                "default-src 'self'; img-src 'self' data:; connect-src 'self'; \
                 frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
            ));
        if let Some(cookie) = reply.cookie {
            response = response.with_header(header("Set-Cookie", &cookie));
        }
        let _ = request.respond(response);
    }

    fn reply(&mut self, request: &mut Request) -> Reply {
        if !self.host_allowed(request) {
            return Reply::error(403, "unexpected Host header");
        }
        let (path, query) = split_url(request.url());
        let method = request.method().clone();
        let mut cookie = None;
        if !self.has_session(request) {
            let presented = query.iter().find(|(k, _)| k == "token").map(|(_, v)| v);
            match (&self.token, presented) {
                (Some(token), Some(given))
                    if method == Method::Get && constant_time_eq(token, given) =>
                {
                    self.token = None;
                    let session = match random_hex() {
                        Ok(session) => session,
                        Err(e) => return Reply::error(500, &e.to_string()),
                    };
                    cookie = Some(format!(
                        "{COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict"
                    ));
                    self.sessions.push(session);
                }
                _ => return Reply::error(401, "unauthorized: open the URL printed by `muster ui`"),
            }
        }
        let mut reply = self.route(request, &method, &path, &query);
        reply.cookie = cookie;
        reply
    }

    fn route(
        &mut self,
        request: &mut Request,
        method: &Method,
        path: &str,
        query: &[(String, String)],
    ) -> Reply {
        match (method, path) {
            (Method::Get, "/") | (Method::Get, "/index.html") => {
                Reply::new(200, "text/html; charset=utf-8", INDEX_HTML)
            }
            (Method::Get, "/style.css") => Reply::new(200, "text/css; charset=utf-8", STYLE_CSS),
            (Method::Get, "/app.js") => Reply::new(200, "text/javascript; charset=utf-8", APP_JS),
            (_, p) if p.starts_with("/api/") => {
                let name = p["/api/".len()..].trim_matches('/').replace('/', ".");
                match method {
                    Method::Get => {
                        let body = query
                            .iter()
                            .filter(|(k, _)| k != "token")
                            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                            .collect();
                        self.api(&name, false, body)
                    }
                    Method::Post => match read_json(request) {
                        Ok(body) => self.api(&name, true, body),
                        Err(reply) => reply,
                    },
                    _ => Reply::error(405, "method not allowed"),
                }
            }
            _ => Reply::error(404, "not found"),
        }
    }

    fn api(&mut self, name: &str, post: bool, body: Map<String, Value>) -> Reply {
        let name = alias(name);
        match name {
            "history" => return self.history(),
            "secret.reveal" if post => return self.reveal(body),
            "secret.reveal" => return Reply::error(405, "reveal requires POST"),
            _ => {}
        }
        let Some(spec) = registry::find(name).filter(|s| !BLOCKED.contains(&s.name)) else {
            return Reply::error(404, &format!("unknown API command '{name}'"));
        };
        if spec.mutating && !post {
            return Reply::error(405, &format!("'{name}' changes state: use POST"));
        }
        let (params, flags) = match parse_params(spec, body) {
            Ok(parsed) => parsed,
            Err(e) => return self.failure(spec, &cli_line(spec, &[], Flags::default()), e),
        };
        self.execute(spec, &params, flags)
    }

    /// Run a registry handler exactly like the CLI does (same globals, no TTY).
    fn execute(&self, spec: &CommandSpec, params: &[(String, Value)], flags: Flags) -> Reply {
        let cli = cli_line(spec, params, flags);
        let mut typed = Params::default();
        for (k, v) in params {
            typed.set(k.clone(), v.clone());
        }
        let globals = Globals {
            config: Some(self.config_dir.clone()),
            format: Some(Format::Json),
            yes: flags.confirm,
            dry_run: flags.dry_run,
            quiet: true,
            verbose: false,
        };
        let mut ctx = match Context::new(globals) {
            Ok(ctx) => ctx,
            Err(e) => return self.failure(spec, &cli, e),
        };
        // Never prompt on the server's terminal: confirmation comes from the body.
        ctx.is_tty = false;
        let result = (spec.handler)(&mut ctx, &typed);
        let mut diagnostics = ctx.take_diagnostics();
        let (ok, result, status) = match result {
            Ok(value) => (!ctx.failed(), value, 200),
            Err(e) => {
                diagnostics.push(e.to_diagnostic());
                (false, Value::Null, http_status(e.exit_code()))
            }
        };
        envelope(spec.name, ok, result, diagnostics, &cli, status)
    }

    fn failure(&self, spec: &CommandSpec, cli: &str, err: MusterError) -> Reply {
        let status = http_status(err.exit_code());
        envelope(
            spec.name,
            false,
            Value::Null,
            vec![err.to_diagnostic()],
            cli,
            status,
        )
    }

    fn reveal(&mut self, body: Map<String, Value>) -> Reply {
        let spec = registry::find("secret.get").expect("secret.get is registered");
        let name = body.get("name").and_then(Value::as_str).unwrap_or_default();
        let reference = if name.contains("://") {
            name.to_owned()
        } else {
            format!("native://{name}")
        };
        let params = vec![
            ("reference".to_owned(), Value::from(reference)),
            ("reveal".to_owned(), Value::Bool(true)),
        ];
        let flags = Flags {
            confirm: true,
            dry_run: false,
        };
        if name.is_empty() {
            let err = MusterError::Usage("reveal needs the secret \"name\"".into());
            return self.failure(spec, &cli_line(spec, &params, flags), err);
        }
        if body.get(CONFIRM).and_then(Value::as_bool) != Some(true) {
            let err = MusterError::Usage(
                "revealing a secret requires explicit confirmation: {\"confirm\": true}".into(),
            );
            return self.failure(spec, &cli_line(spec, &params, flags), err);
        }
        // `secret get --reveal` records the reveal in the local history.
        self.execute(spec, &params, flags)
    }

    fn history(&self) -> Reply {
        let cli = "muster rollback <generation>";
        let result = crate::engine::generations(&self.state_dir).and_then(|generations| {
            Ok(json!({
                "generations": generations,
                "events": crate::audit::events(&self.state_dir)?,
            }))
        });
        match result {
            Ok(result) => envelope("history", true, result, Vec::new(), cli, 200),
            Err(e) => {
                let status = http_status(e.exit_code());
                envelope(
                    "history",
                    false,
                    Value::Null,
                    vec![e.to_diagnostic()],
                    cli,
                    status,
                )
            }
        }
    }

    fn has_session(&self, request: &Request) -> bool {
        request
            .headers()
            .iter()
            .filter(|h| h.field.equiv("Cookie"))
            .flat_map(|h| h.value.as_str().split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(k, v)| k == COOKIE && self.sessions.iter().any(|s| constant_time_eq(s, v)))
    }

    /// Reject DNS-rebinding: the browser must be talking to us by loopback name.
    fn host_allowed(&self, request: &Request) -> bool {
        let allowed = [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ];
        let host = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Host"))
            .map(|h| h.value.as_str().to_owned());
        let origin_ok = request
            .headers()
            .iter()
            .filter(|h| h.field.equiv("Origin"))
            .all(|h| {
                allowed
                    .iter()
                    .any(|a| h.value.as_str() == format!("http://{a}"))
            });
        origin_ok && host.is_some_and(|h| allowed.contains(&h))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    confirm: bool,
    dry_run: bool,
}

/// Friendly view names → registry commands.
fn alias(name: &str) -> &str {
    match name {
        "machines" => "machine.list",
        "members" => "member.list",
        "groups" => "group.list",
        "secrets" => "secret.list",
        other => other,
    }
}

/// JSON body → typed params in spec order, plus the confirm/dry-run flags.
/// Unknown keys and wrong types are usage errors (exit 2 → HTTP 400).
fn parse_params(
    spec: &CommandSpec,
    body: Map<String, Value>,
) -> Result<(Vec<(String, Value)>, Flags), MusterError> {
    let mut flags = Flags::default();
    let mut given = Map::new();
    for (key, value) in body {
        match key.as_str() {
            CONFIRM => flags.confirm = as_bool(&value, &key)?,
            DRY_RUN | "dry-run" => flags.dry_run = as_bool(&value, &key)?,
            _ => {
                let canonical = key.replace('_', "-");
                if !spec.params.iter().any(|p| p.name == canonical) {
                    return Err(MusterError::Usage(format!(
                        "unknown parameter '{key}' for '{}'",
                        spec.name
                    )));
                }
                given.insert(canonical, value);
            }
        }
    }
    let mut params = Vec::new();
    for p in spec.params {
        let Some(value) = given.remove(p.name) else {
            if p.required {
                return Err(MusterError::Usage(format!(
                    "missing required parameter '{}'",
                    p.name
                )));
            }
            continue;
        };
        let bad = || MusterError::Usage(format!("parameter '{}' must be a {:?}", p.name, p.kind));
        let value = match (p.kind, value) {
            (ParamKind::String, Value::String(s)) => Value::String(s),
            (ParamKind::Bool, v) => Value::Bool(as_bool(&v, p.name)?),
            (ParamKind::Int, Value::Number(n)) if n.is_i64() => Value::Number(n),
            (ParamKind::Int, Value::String(s)) => Value::from(s.parse::<i64>().map_err(|_| bad())?),
            (ParamKind::List, Value::String(s)) => Value::Array(vec![Value::String(s)]),
            (ParamKind::List, Value::Array(items)) if items.iter().all(Value::is_string) => {
                Value::Array(items)
            }
            _ => return Err(bad()),
        };
        params.push((p.name.to_owned(), value));
    }
    Ok((params, flags))
}

fn as_bool(value: &Value, key: &str) -> Result<bool, MusterError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s == "true" => Ok(true),
        Value::String(s) if s == "false" => Ok(false),
        _ => Err(MusterError::Usage(format!("'{key}' must be true or false"))),
    }
}

/// The exact command line equivalent to an API call.
fn cli_line(spec: &CommandSpec, params: &[(String, Value)], flags: Flags) -> String {
    let mut words: Vec<String> = vec!["muster".into()];
    words.extend(spec.name.split('.').map(str::to_owned));
    for p in spec.params {
        let Some((_, value)) = params.iter().find(|(k, _)| k == p.name) else {
            continue;
        };
        let flag = format!("--{}", p.name);
        match value {
            Value::Bool(true) => words.push(flag),
            Value::Bool(false) => {}
            Value::Array(items) => {
                for item in items {
                    words.push(flag.clone());
                    words.push(shell_quote(&scalar(item)));
                }
            }
            other if p.positional => words.push(shell_quote(&scalar(other))),
            other => {
                words.push(flag);
                words.push(shell_quote(&scalar(other)));
            }
        }
    }
    if flags.dry_run {
        words.push("--dry-run".into());
    }
    if flags.confirm {
        words.push("--yes".into());
    }
    words.join(" ")
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:@=+,%".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn envelope(
    command: &str,
    ok: bool,
    result: Value,
    diagnostics: Vec<crate::output::Diagnostic>,
    cli: &str,
    status: u16,
) -> Reply {
    let envelope = Envelope::new(command, ok, result, diagnostics);
    let mut value = serde_json::to_value(&envelope).expect("envelope serializes");
    value["cli"] = Value::from(cli);
    Reply::json(status, value)
}

fn http_status(exit_code: i32) -> u16 {
    match exit_code {
        2 => 400,
        3 => 422,
        4 => 503,
        5 => 403,
        _ => 500,
    }
}

fn read_json(request: &mut Request) -> Result<Map<String, Value>, Reply> {
    let json_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .is_some_and(|h| h.value.as_str().starts_with("application/json"));
    if !json_type {
        return Err(Reply::error(415, "POST bodies must be application/json"));
    }
    let mut text = String::new();
    request
        .as_reader()
        .take(MAX_BODY)
        .read_to_string(&mut text)
        .map_err(|_| Reply::error(400, "unreadable body"))?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(Reply::error(400, "the body must be a JSON object")),
    }
}

fn split_url(url: &str) -> (String, Vec<(String, String)>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let pairs = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    (percent_decode(path), pairs)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => match bytes.get(i + 1..i + 3) {
                Some(h) if h.iter().all(u8::is_ascii_hexdigit) => {
                    let h = std::str::from_utf8(h).expect("hex digits are ASCII");
                    out.push(u8::from_str_radix(h, 16).expect("two hex digits"));
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header is valid")
}

/// 128 random bits as 32 lowercase hex characters, from the OS CSPRNG.
fn random_hex() -> Result<String, MusterError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| MusterError::Operational(format!("cannot read OS randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_escapes_and_stray_percent() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn shell_quoting_keeps_plain_words_and_quotes_the_rest() {
        assert_eq!(shell_quote("native://app/api"), "native://app/api");
        assert_eq!(shell_quote("it's x"), r"'it'\''s x'");
    }
}
