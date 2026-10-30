//! The web admin interface behind `muster ui`.
//!
//! - Listens on 127.0.0.1 only. Startup prints `http://127.0.0.1:<port>/?token=<t>`;
//!   the one-time token is consumed by the first request that presents it and
//!   exchanged for an HttpOnly, SameSite=Strict session cookie. Everything else
//!   without a valid session answers 401.
//! - Every asset is embedded in the binary; the CSP allows only this origin.
//! - The API is an explicit table (`ROUTES`) over the registry: each route runs
//!   its command through `cli::execute`, exactly like the CLI, and returns the
//!   envelope plus `cli`, the equivalent command line. Reads are GET, changes
//!   POST; confirmations need `{"confirm": true}` (the CLI's `--yes`).
//!   Profile and MCP-server edits post the same TOML text and KEY=VALUE lists
//!   the CLI accepts; the command stores them verbatim, so secret references
//!   stay references and no secret value ever needs to be sent.
//! - `POST /api/secret/reveal {"name", "confirm": true}` is the only path that
//!   returns a secret value, and it is recorded in the local history first.
//! - `POST /api/secret/add|rotate` accepts `path`, `value` and `confirm: true`.
//!   Values go directly to the shared vault handlers, never command parameters,
//!   responses or history; the CLI equivalent uses `--value-stdin`.

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

/// One API route: `GET|POST /api/<path>` runs `command`. Commands that read
/// stdin, bootstrap identities or start servers (`init`, `ui`, `mcp`) are
/// absent. Secret writes use a dedicated body-only path; values only leave
/// through the audited `secret/reveal` route.
struct Route {
    path: &'static str,
    command: &'static str,
    post: bool,
    /// Parameters this route does not accept (e.g. `doctor --fix` on GET, or
    /// `profile set --from-file`, which must never read local files).
    hidden: &'static [&'static str],
}

const fn get(path: &'static str, command: &'static str) -> Route {
    Route {
        path,
        command,
        post: false,
        hidden: &[],
    }
}

const fn post(path: &'static str, command: &'static str) -> Route {
    Route {
        path,
        command,
        post: true,
        hidden: &[],
    }
}

const ROUTES: &[Route] = &[
    get("status", "status"),
    get("effective", "effective"),
    get("machines", "machine.list"),
    get("members", "member.list"),
    get("groups", "group.list"),
    get("profiles", "profile.list"),
    get("providers", "provider.list"),
    Route {
        hidden: &["value-stdin"],
        ..post("provider/add", "provider.add")
    },
    post("provider/rm", "provider.rm"),
    post("provider/use", "provider.use"),
    get("profile", "profile.show"),
    get("secrets", "secret.list"),
    get("secret/requests", "secret.requests"),
    get("secret/leases", "secret.leases"),
    post("secret/approve", "secret.approve"),
    post("secret/deny", "secret.deny"),
    post("secret/revoke", "secret.revoke"),
    get("plan", "plan"),
    get("proposals", "proposal.list"),
    get("mcp-servers", "mcp-server.list"),
    get("schedule", "schedule.status"),
    Route {
        hidden: &["fix"],
        ..get("doctor", "doctor")
    },
    post("doctor", "doctor"),
    post("sync", "sync"),
    post("rollback", "rollback"),
    post("approve", "approve"),
    post("reject", "reject"),
    post("revoke", "revoke"),
    post("machine/assign", "machine.assign"),
    post("machine/rm", "machine.rm"),
    post("member/add", "member.add"),
    post("member/approve", "member.approve"),
    post("member/reject", "member.reject"),
    post("member/role", "member.role"),
    post("member/profile", "member.profile"),
    post("member/rm", "member.rm"),
    post("group/create", "group.create"),
    post("group/grant", "group.grant"),
    post("group/revoke", "group.revoke"),
    // `--from-file` would let a POST read arbitrary local files; the panel
    // edits profiles only through the inline `--toml` text.
    Route {
        hidden: &["from-file"],
        ..post("profile/set", "profile.set")
    },
    post("profile/rm", "profile.rm"),
    post("mcp-server/add", "mcp-server.add"),
    post("mcp-server/rm", "mcp-server.rm"),
    post("secret/rm", "secret.rm"),
    post("proposal/create", "proposal.create"),
    post("proposal/accept", "proposal.accept"),
    get("skills", "skill.list"),
    post("skills/check", "skill.check"),
    post("skills/add", "skill.add"),
    post("skills/update", "skill.update"),
    post("skills/rm", "skill.rm"),
    post("schedule/enable", "schedule.enable"),
    post("schedule/disable", "schedule.disable"),
    // `--project` would let the panel edit any directory's `muster.toml`;
    // the panel toggles only the shared configuration.
    Route {
        hidden: &["project"],
        ..post("enable", "enable")
    },
    Route {
        hidden: &["project"],
        ..post("disable", "disable")
    },
];
/// Reference schemes the panel may reveal. `env://` is excluded: it would read
/// the panel process's own environment.
const REVEAL_SCHEMES: &[&str] = &["native", "op", "bws", "doppler", "infisical"];

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
                let name = p["/api/".len()..].trim_matches('/').to_owned();
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
        match (name, post) {
            ("history", false) => return self.history(),
            ("secret/reveal", true) => return self.reveal(body),
            ("secret/add" | "secret/rotate", true) => return self.write_secret(name, body),
            _ => {}
        }
        let Some(route) = ROUTES.iter().find(|r| r.path == name && r.post == post) else {
            let other = ROUTES.iter().any(|r| r.path == name) || name == "history";
            return if other || matches!(name, "secret/reveal" | "secret/add" | "secret/rotate") {
                Reply::error(405, &format!("wrong method for /api/{name}"))
            } else {
                Reply::error(404, &format!("unknown API route /api/{name}"))
            };
        };
        let spec = registry::find(route.command).expect("routes map to registered commands");
        let (params, flags) = match parse_params(spec, route.hidden, body) {
            Ok(parsed) => parsed,
            Err(e) => return self.failure(spec, &cli_line(spec, &[], Flags::default()), e),
        };
        self.execute(spec, &params, flags)
    }

    /// Run a command exactly like the CLI does (`cli::execute`, never prompting:
    /// confirmation comes from the body).
    fn execute(
        &self,
        spec: &'static CommandSpec,
        params: &[(String, Value)],
        flags: Flags,
    ) -> Reply {
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
            audit_actor: None,
            web_approval: flags.confirm,
        };
        let (envelope, code) = crate::cli::execute(spec, globals, &typed, false);
        // A command that ran but reported problems (e.g. doctor) keeps its result.
        let status = if code == 0 || !envelope.result.is_null() {
            200
        } else {
            http_status(code)
        };
        reply(&envelope, &cli, status)
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

    fn write_secret(&self, route: &str, mut body: Map<String, Value>) -> Reply {
        use crate::commands::secret;
        use crate::secrets::SecretValue;
        let spec = if route == "secret/add" {
            &secret::ADD
        } else {
            &secret::ROTATE
        };
        // Move the value into zeroizing storage before validating anything else.
        let value = match body.remove("value") {
            Some(Value::String(value)) => SecretValue::new(value),
            _ => return Reply::error(400, "value must be a string"),
        };
        if body.get(CONFIRM).and_then(Value::as_bool) != Some(true) {
            return Reply::error(400, "secret writes require confirm: true");
        }
        if !body.contains_key("path") {
            if let Some(name) = body.remove("name") {
                let path = match (body.remove("group"), name) {
                    (Some(Value::String(group)), Value::String(name)) => {
                        Value::String(format!("{group}/{name}"))
                    }
                    (None, name) => name,
                    _ => return Reply::error(400, "name and group must be strings"),
                };
                body.insert("path".into(), path);
            }
        }
        let (mut params, flags) = match parse_params(spec, &[], body) {
            Ok(parsed) => parsed,
            Err(e) => return self.failure(spec, &cli_line(spec, &[], Flags::default()), e),
        };
        params.push(("value-stdin".into(), Value::Bool(true)));
        let cli = cli_line(spec, &params, flags);
        let mut typed = Params::default();
        for (k, v) in params {
            typed.set(k, v);
        }
        let globals = Globals {
            config: Some(self.config_dir.clone()),
            format: Some(Format::Json),
            yes: true,
            dry_run: flags.dry_run,
            quiet: true,
            verbose: false,
            audit_actor: None,
            web_approval: false,
        };
        let (envelope, code) = crate::cli::execute_with(spec, globals, false, |ctx| {
            if route == "secret/add" {
                secret::add_with_value(ctx, &typed, value)
            } else {
                secret::rotate_with_value(ctx, &typed, value)
            }
        });
        reply(
            &envelope,
            &cli,
            if code == 0 { 200 } else { http_status(code) },
        )
    }

    fn reveal(&mut self, body: Map<String, Value>) -> Reply {
        let spec = registry::find("secret.get").expect("secret.get is registered");
        let name = body.get("name").and_then(Value::as_str).unwrap_or_default();
        let reference = if let Some((scheme, _)) = name.split_once("://") {
            if !REVEAL_SCHEMES.contains(&scheme) {
                let params = vec![("reference".to_owned(), Value::from(name))];
                let err = MusterError::Usage(format!(
                    "the panel cannot reveal '{scheme}://' references; allowed: {}",
                    REVEAL_SCHEMES.join(", ")
                ));
                return self.failure(spec, &cli_line(spec, &params, Flags::default()), err);
            }
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

/// JSON body → typed params in spec order, plus the confirm/dry-run flags.
/// Unknown keys and wrong types are usage errors (exit 2 → HTTP 400).
fn parse_params(
    spec: &CommandSpec,
    hidden: &[&str],
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
                let known = spec.params.iter().any(|p| p.name == canonical);
                // `*-stdin` would read the server's own stdin, never a request.
                if !known || hidden.contains(&canonical.as_str()) || canonical.ends_with("-stdin") {
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
                    let text = scalar(item);
                    if text.starts_with('-') {
                        // `--args -y` reads `-y` as a flag: attach the value.
                        words.push(format!("{flag}={}", shell_quote(&text)));
                    } else {
                        words.push(flag.clone());
                        words.push(shell_quote(&text));
                    }
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

fn reply(envelope: &Envelope, cli: &str, status: u16) -> Reply {
    let mut value = serde_json::to_value(envelope).expect("envelope serializes");
    value["cli"] = Value::from(cli);
    Reply::json(status, value)
}

fn envelope(
    command: &str,
    ok: bool,
    result: Value,
    diagnostics: Vec<crate::output::Diagnostic>,
    cli: &str,
    status: u16,
) -> Reply {
    reply(
        &Envelope::new(command, ok, result, diagnostics),
        cli,
        status,
    )
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
    let mut text = secrecy::zeroize::Zeroizing::new(Vec::new());
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut text)
        .map_err(|_| Reply::error(400, "unreadable body"))?;
    if text.len() as u64 > MAX_BODY {
        return Err(Reply::error(413, "body exceeds 64 KB"));
    }
    if text.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice(&text) {
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

    #[test]
    fn cli_line_attaches_hyphen_leading_list_values_with_equals() {
        // `muster mcp-server add --args -y` would parse `-y` as a flag; the
        // CLI-equivalent line must use `--args=-y` for such values.
        let args = Value::Array(vec![Value::from("-y"), Value::from("server-name")]);
        let params = vec![("args".to_owned(), args)];
        let cli = cli_line(&crate::commands::mcp_server::ADD, &params, Flags::default());
        assert_eq!(cli, "muster mcp-server add --args=-y --args server-name");
    }
}
