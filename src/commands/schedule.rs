//! `muster schedule enable|disable|status`: periodic `muster sync` through a
//! systemd user timer (Linux/WSL) or a launchd agent (macOS), with manual
//! instructions when neither is available.
//!
//! The scheduled run never passes `--yes`: new hooks and MCP servers still
//! need a person to approve them, so an unattended sync refuses (exit 2)
//! instead of installing them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::machine;
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

const SERVICE: &str = "muster-sync.service";
const TIMER: &str = "muster-sync.timer";
const LAUNCHD_LABEL: &str = "dev.muster.sync";
const DEFAULT_INTERVAL_MINUTES: i64 = 60;

pub const ENABLE: CommandSpec = CommandSpec {
    name: "schedule.enable",
    about: "Run `muster sync` periodically (systemd user timer, launchd agent or manual)",
    params: &[
        ParamSpec {
            name: "interval",
            about: "Minutes between syncs (default 60)",
            kind: ParamKind::Int,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "backend",
            about: "auto (default), systemd, launchd or manual",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: enable,
};

pub const DISABLE: CommandSpec = CommandSpec {
    name: "schedule.disable",
    about: "Stop periodic sync and remove Muster's scheduler units",
    params: &[],
    mutating: true,
    handler: disable,
};

pub const STATUS: CommandSpec = CommandSpec {
    name: "schedule.status",
    about: "Show whether periodic sync is scheduled and by which backend",
    params: &[],
    mutating: false,
    handler: status,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Systemd,
    Launchd,
    Manual,
}

impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Backend::Systemd => "systemd",
            Backend::Launchd => "launchd",
            Backend::Manual => "manual",
        }
    }

    fn parse(raw: Option<&str>) -> Result<Self, MusterError> {
        match raw.unwrap_or("auto") {
            "auto" if cfg!(target_os = "macos") => Ok(Backend::Launchd),
            "auto" if machine::find_executable("systemctl").is_some() => Ok(Backend::Systemd),
            "auto" | "manual" => Ok(Backend::Manual),
            "systemd" => Ok(Backend::Systemd),
            "launchd" => Ok(Backend::Launchd),
            other => Err(MusterError::Usage(format!(
                "invalid backend '{other}': expected auto, systemd, launchd or manual"
            ))),
        }
    }
}

/// Where each backend keeps Muster's unit files.
struct Units {
    systemd_dir: PathBuf,
    plist: PathBuf,
}

impl Units {
    fn locate() -> Result<Self, MusterError> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| MusterError::Operational("HOME is not set".into()))?;
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Ok(Self {
            systemd_dir: config.join("systemd/user"),
            plist: home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")),
        })
    }

    fn systemd_files(&self) -> [PathBuf; 2] {
        [self.systemd_dir.join(SERVICE), self.systemd_dir.join(TIMER)]
    }

    /// Unit files that exist now, with their backend.
    fn installed(&self) -> Vec<(Backend, PathBuf)> {
        let systemd = self
            .systemd_files()
            .into_iter()
            .map(|p| (Backend::Systemd, p));
        let launchd = std::iter::once((Backend::Launchd, self.plist.clone()));
        systemd
            .chain(launchd)
            .filter(|(_, p)| p.is_file())
            .collect()
    }
}

/// The scheduled command line: this binary, this config repo, `sync`.
fn sync_command(ctx: &Context) -> Result<Vec<String>, MusterError> {
    let exe = std::env::current_exe()
        .map_err(|e| MusterError::Operational(format!("cannot locate the muster binary: {e}")))?;
    let config = std::path::absolute(&ctx.config_dir)
        .map_err(|e| MusterError::Operational(format!("{}: {e}", ctx.config_dir.display())))?;
    Ok(vec![
        exe.display().to_string(),
        "--config".into(),
        config.display().to_string(),
        "sync".into(),
        "--quiet".into(),
    ])
}

fn enable(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let minutes = params
        .get_int("interval")
        .unwrap_or(DEFAULT_INTERVAL_MINUTES);
    if minutes < 1 {
        return Err(MusterError::Usage(
            "interval must be at least 1 minute".into(),
        ));
    }
    let backend = Backend::parse(params.get_string("backend"))?;
    let command = sync_command(ctx)?;
    let units = Units::locate()?;

    let files: Vec<(PathBuf, String)> = match backend {
        Backend::Manual => {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "scheduler_manual",
                    "neither systemd nor launchd is available: run `muster sync` periodically yourself",
                )
                .with_suggestion(format!(
                    "e.g. a cron entry: `0 * * * * {}`",
                    shell_line(&command)
                )),
            );
            return Ok(json!({
                "backend": backend.as_str(),
                "enabled": false,
                "interval_minutes": minutes,
                "command": command,
            }));
        }
        Backend::Systemd => {
            let [service, timer] = units.systemd_files();
            vec![
                (service, systemd_service(&command)),
                (timer, systemd_timer(minutes)),
            ]
        }
        Backend::Launchd => vec![(
            units.plist.clone(),
            launchd_plist(&command, minutes, &ctx.state_dir),
        )],
    };
    let paths: Vec<&PathBuf> = files.iter().map(|(p, _)| p).collect();
    let result = |enabled: bool| {
        json!({
            "backend": backend.as_str(),
            "enabled": enabled,
            "interval_minutes": minutes,
            "command": command,
            "files": paths,
        })
    };
    if ctx.dry_run {
        return Ok(result(false));
    }

    for (path, text) in &files {
        write_file(path, text)?;
    }
    match backend {
        Backend::Systemd => activate(
            ctx,
            "systemctl",
            &[
                &["--user", "daemon-reload"],
                &["--user", "enable", "--now", TIMER],
            ],
        ),
        Backend::Launchd => {
            let plist = units.plist.display().to_string();
            activate(ctx, "launchctl", &[&["load", "-w", &plist]]);
        }
        Backend::Manual => unreachable!("handled above"),
    }
    Ok(result(true))
}

fn disable(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let removed = remove_units(ctx)?;
    Ok(json!({ "enabled": false, "removed": removed, "dry_run": ctx.dry_run }))
}

fn status(_ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let installed = Units::locate()?.installed();
    let backend = installed.first().map(|(b, _)| b.as_str());
    let files: Vec<&PathBuf> = installed.iter().map(|(_, p)| p).collect();
    Ok(json!({
        "enabled": !installed.is_empty(),
        "backend": backend,
        "files": files,
    }))
}

/// Stop and delete every Muster scheduler unit (all backends). Honours
/// `--dry-run`. Shared with `muster uninstall`.
pub fn remove_units(ctx: &mut Context) -> Result<Vec<PathBuf>, MusterError> {
    let units = Units::locate()?;
    let installed = units.installed();
    if ctx.dry_run {
        return Ok(installed.into_iter().map(|(_, p)| p).collect());
    }
    if installed.iter().any(|(b, _)| *b == Backend::Systemd) {
        activate(ctx, "systemctl", &[&["--user", "disable", "--now", TIMER]]);
    }
    if installed.iter().any(|(b, _)| *b == Backend::Launchd) {
        let plist = units.plist.display().to_string();
        activate(ctx, "launchctl", &[&["unload", "-w", &plist]]);
    }
    let mut removed = Vec::new();
    for (_, path) in installed {
        std::fs::remove_file(&path)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
        removed.push(path);
    }
    if removed.iter().any(|p| p.starts_with(&units.systemd_dir)) {
        activate(ctx, "systemctl", &[&["--user", "daemon-reload"]]);
    }
    Ok(removed)
}

/// Run the backend's control tool if present. Failures are warnings: the
/// unit files are in place and the user can load them by hand.
fn activate(ctx: &mut Context, tool: &str, calls: &[&[&str]]) {
    let Some(exe) = machine::find_executable(tool) else {
        let commands: Vec<String> = calls
            .iter()
            .map(|c| format!("{tool} {}", c.join(" ")))
            .collect();
        ctx.diag(Diagnostic::new(
            Severity::Info,
            "scheduler_not_loaded",
            format!("`{tool}` is not on PATH; run: {}", commands.join(" && ")),
        ));
        return;
    };
    for args in calls {
        let output = Command::new(&exe).args(*args).stdin(Stdio::null()).output();
        let failure = match output {
            Ok(o) if o.status.success() => continue,
            Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => e.to_string(),
        };
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "scheduler_activation_failed",
            format!("`{tool} {}` failed: {failure}", args.join(" ")),
        ));
        return;
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), MusterError> {
    let io = |e: std::io::Error| MusterError::Operational(format!("{}: {e}", path.display()));
    std::fs::create_dir_all(path.parent().expect("unit path has a parent")).map_err(io)?;
    std::fs::write(path, text).map_err(io)
}

/// systemd `ExecStart`: every argument double-quoted, `%` doubled.
fn systemd_service(command: &[String]) -> String {
    let exec: Vec<String> = command
        .iter()
        .map(|a| {
            let escaped = a
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%");
            format!("\"{escaped}\"")
        })
        .collect();
    format!(
        "# Managed by Muster (`muster schedule`). Removed by `muster schedule disable`.\n\
         [Unit]\n\
         Description=Muster periodic sync\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={}\n",
        exec.join(" ")
    )
}

fn systemd_timer(minutes: i64) -> String {
    let cadence = if minutes == DEFAULT_INTERVAL_MINUTES {
        "OnCalendar=hourly".to_owned()
    } else {
        format!("OnBootSec=5min\nOnUnitActiveSec={minutes}min")
    };
    format!(
        "# Managed by Muster (`muster schedule`). Removed by `muster schedule disable`.\n\
         [Unit]\n\
         Description=Run Muster sync every {minutes} minutes\n\
         \n\
         [Timer]\n\
         {cadence}\n\
         Persistent=true\n\
         RandomizedDelaySec=300\n\
         Unit={SERVICE}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

fn launchd_plist(command: &[String], minutes: i64, state_dir: &Path) -> String {
    let args: String = command
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let log = xml_escape(&state_dir.join("schedule.log").display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Managed by Muster (`muster schedule`). Removed by `muster schedule disable`. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {args}\
         \x20 </array>\n\
         \x20 <key>StartInterval</key>\n\
         \x20 <integer>{}</integer>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <false/>\n\
         \x20 <key>StandardOutPath</key>\n\
         \x20 <string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key>\n\
         \x20 <string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        minutes * 60
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// POSIX-shell rendering of `command` for the manual instructions.
fn shell_line(command: &[String]) -> String {
    command
        .iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timer_is_persistent_hourly_with_jitter() {
        let timer = systemd_timer(DEFAULT_INTERVAL_MINUTES);
        assert!(timer.contains("OnCalendar=hourly\n"));
        assert!(timer.contains("Persistent=true\n"));
        assert!(timer.contains("RandomizedDelaySec=300\n"));
    }

    #[test]
    fn launchd_runs_hourly_and_escapes_arguments() {
        let plist = launchd_plist(
            &["/bin/muster&test".into(), "sync".into()],
            60,
            Path::new("/tmp/state"),
        );
        assert!(plist.contains("<integer>3600</integer>"));
        assert!(plist.contains("/bin/muster&amp;test"));
    }
}
