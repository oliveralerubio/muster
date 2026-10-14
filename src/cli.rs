//! Thin CLI adapter: the clap parser is built from `registry::COMMANDS`, so the
//! CLI, MCP, web API and `muster schema` all describe the same commands.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::Value;

use crate::context::{Context, Globals};
use crate::errors::MusterError;
use crate::output::{self, Envelope, Format};
use crate::registry::{self, CommandSpec, ParamKind, Params};

/// Command name used in the envelope when no command could be resolved.
const ROOT: &str = "muster";

/// Parse `args` (including argv[0]), run the command, print the result and
/// return the process exit code.
pub fn main<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let matches = match command().try_get_matches_from(&args) {
        Ok(matches) => matches,
        Err(err) => return clap_failure(&err, &args),
    };
    let (name, sub) = leaf(&matches);
    let spec = registry::find(&name).expect("parser is built from the registry");
    let globals = globals(&matches);
    if spec.name == crate::mcp::SPEC.name {
        return crate::mcp::serve(globals);
    }
    let (format, quiet) = (globals.format.unwrap_or(Format::Text), globals.quiet);
    let (envelope, code) = execute(spec, globals, &params(spec, sub), true);
    output::render(&envelope, format, quiet);
    code
}

/// Run one registry command and build its envelope and exit code. Shared by
/// the CLI and the MCP server so both report exactly the same result. With
/// `interactive = false` confirmations never prompt (`--yes` or exit 2).
pub fn execute(
    spec: &'static CommandSpec,
    globals: Globals,
    params: &Params,
    interactive: bool,
) -> (Envelope, i32) {
    let (result, mut diagnostics, failed) = match Context::new(globals) {
        Ok(mut ctx) => {
            ctx.is_tty &= interactive;
            let result = (spec.handler)(&mut ctx, params);
            (result, ctx.take_diagnostics(), ctx.failed())
        }
        Err(err) => (Err(err), Vec::new(), false),
    };
    let (ok, result, code) = match result {
        Ok(value) if failed => (false, value, 1),
        Ok(value) => (true, value, 0),
        Err(err) => {
            diagnostics.push(err.to_diagnostic());
            (false, Value::Null, err.exit_code())
        }
    };
    (Envelope::new(spec.name, ok, result, diagnostics), code)
}

/// The full clap parser: global options plus one (nested) subcommand per spec.
pub fn command() -> Command {
    let flag = |name: &'static str, help: &'static str| {
        Arg::new(name)
            .long(name)
            .help(help)
            .action(ArgAction::SetTrue)
            .global(true)
    };
    let root = Command::new(ROOT)
        .about("Sync machines, AI agent configuration and secrets")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("PATH")
                .env("MUSTER_CONFIG")
                .help("Config repository [default: $XDG_CONFIG_HOME/muster]")
                .global(true),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .env("MUSTER_FORMAT")
                .value_parser(["text", "json"])
                .default_value("text")
                .help("Output format")
                .global(true),
        )
        .arg(flag(
            "yes",
            "Assume yes for confirmations (required without a TTY)",
        ))
        .arg(flag(
            "dry-run",
            "Show what would change without writing anything",
        ))
        .arg(flag("quiet", "Silence progress output").conflicts_with("verbose"))
        .arg(flag("verbose", "Show detailed progress output"));

    registry::COMMANDS.iter().fold(root, |root, spec| {
        let path: Vec<&'static str> = spec.name.split('.').collect();
        insert(root, &path, spec)
    })
}

/// Insert `spec` under `parent` following its dotted path, creating group
/// commands (`secret` for `secret.add`) on the way. A spec named like an
/// existing group (`agents` next to `agents.apply`) makes the group runnable.
fn insert(parent: Command, path: &[&'static str], spec: &'static CommandSpec) -> Command {
    match path {
        [] => parent,
        [name] if parent.find_subcommand(name).is_some() => parent.mut_subcommand(name, |group| {
            with_params(
                group
                    .about(spec.about)
                    .subcommand_required(false)
                    .arg_required_else_help(false),
                spec,
            )
        }),
        [name] => parent.subcommand(leaf_command(name, spec)),
        [group, rest @ ..] => {
            let parent = if parent.find_subcommand(group).is_some() {
                parent
            } else {
                parent.subcommand(
                    Command::new(*group)
                        .subcommand_required(true)
                        .arg_required_else_help(true),
                )
            };
            parent.mut_subcommand(group, |sub| insert(sub, rest, spec))
        }
    }
}

fn leaf_command(name: &'static str, spec: &'static CommandSpec) -> Command {
    with_params(Command::new(name).about(spec.about), spec)
}

fn with_params(cmd: Command, spec: &'static CommandSpec) -> Command {
    spec.params.iter().fold(cmd, |cmd, p| {
        let arg = Arg::new(p.name).help(p.about).required(p.required);
        let arg = if p.positional { arg } else { arg.long(p.name) };
        cmd.arg(match p.kind {
            ParamKind::String => arg,
            ParamKind::Bool => arg.action(ArgAction::SetTrue),
            ParamKind::Int => arg.value_parser(clap::value_parser!(i64)),
            ParamKind::List => arg.action(ArgAction::Append),
        })
    })
}

/// Walk to the innermost subcommand; returns its dotted name and matches.
fn leaf(matches: &ArgMatches) -> (String, &ArgMatches) {
    let mut names = Vec::new();
    let mut current = matches;
    while let Some((name, sub)) = current.subcommand() {
        names.push(name);
        current = sub;
    }
    (names.join("."), current)
}

fn globals(m: &ArgMatches) -> Globals {
    Globals {
        config: m.get_one::<String>("config").map(PathBuf::from),
        format: m.get_one::<String>("format").map(|f| parse_format(f)),
        yes: m.get_flag("yes"),
        dry_run: m.get_flag("dry-run"),
        quiet: m.get_flag("quiet"),
        verbose: m.get_flag("verbose"),
    }
}

fn params(spec: &CommandSpec, m: &ArgMatches) -> Params {
    let mut params = Params::default();
    for p in spec.params {
        let value = match p.kind {
            ParamKind::String => m.get_one::<String>(p.name).map(|s| Value::from(s.as_str())),
            ParamKind::Bool => Some(Value::Bool(m.get_flag(p.name))),
            ParamKind::Int => m.get_one::<i64>(p.name).map(|i| Value::from(*i)),
            ParamKind::List => m
                .get_many::<String>(p.name)
                .map(|vs| vs.map(|s| Value::from(s.as_str())).collect()),
        };
        if let Some(value) = value {
            params.set(p.name, value);
        }
    }
    params
}

fn parse_format(s: &str) -> Format {
    if s == "json" {
        Format::Json
    } else {
        Format::Text
    }
}

/// Help/version print normally (exit 0). Any other parse failure is a usage
/// error (exit 2), wrapped in the JSON envelope when JSON output was asked for.
fn clap_failure(err: &clap::Error, args: &[OsString]) -> i32 {
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        let _ = err.print();
        return 0;
    }
    if requested_format(args) == Format::Text {
        let _ = err.print();
        return 2;
    }
    let usage = MusterError::Usage(clap_message(err));
    let envelope = Envelope::new(ROOT, false, Value::Null, vec![usage.to_diagnostic()]);
    output::render(&envelope, Format::Json, false);
    usage.exit_code()
}

/// First line of clap's message without the `error: ` prefix.
fn clap_message(err: &clap::Error) -> String {
    let rendered = err.render().to_string();
    let line = rendered.lines().next().unwrap_or("invalid usage");
    line.strip_prefix("error: ").unwrap_or(line).to_string()
}

/// Best-effort `--format` lookup when clap could not parse the arguments.
fn requested_format(args: &[OsString]) -> Format {
    let args: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
    let flag = args.iter().enumerate().find_map(|(i, a)| {
        if a == "--format" {
            args.get(i + 1).map(|v| v.to_string())
        } else {
            a.strip_prefix("--format=").map(str::to_string)
        }
    });
    let value = flag.or_else(|| std::env::var("MUSTER_FORMAT").ok());
    value.map_or(Format::Text, |v| parse_format(&v))
}
