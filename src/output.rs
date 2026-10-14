//! The output contract: one JSON envelope on stdout (JSON mode) or plain text;
//! diagnostics and progress always go to stderr in text mode.

use std::io::Write;

use serde::Serialize;
use serde_json::Value;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub severity: Severity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Diagnostic {
    pub fn new(severity: Severity, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            severity,
            path: None,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    pub schema_version: u32,
    pub command: String,
    pub ok: bool,
    pub result: Value,
    pub diagnostics: Vec<Diagnostic>,
}

impl Envelope {
    pub fn new(command: &str, ok: bool, result: Value, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: command.to_string(),
            ok,
            result,
            diagnostics,
        }
    }
}

/// Print an envelope. Write errors (e.g. a closed pipe) are ignored on purpose.
pub fn render(envelope: &Envelope, format: Format, quiet: bool) {
    let mut stdout = std::io::stdout().lock();
    match format {
        Format::Json => {
            let text = serde_json::to_string_pretty(envelope).expect("envelope serializes");
            let _ = writeln!(stdout, "{text}");
        }
        Format::Text => {
            match &envelope.result {
                Value::Null => {}
                Value::String(s) => {
                    let _ = writeln!(stdout, "{s}");
                }
                other => {
                    let text = serde_json::to_string_pretty(other).expect("result serializes");
                    let _ = writeln!(stdout, "{text}");
                }
            }
            let mut stderr = std::io::stderr().lock();
            for d in &envelope.diagnostics {
                if quiet && d.severity != Severity::Error {
                    continue;
                }
                let severity = d.severity.as_str();
                let at = d
                    .path
                    .as_deref()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default();
                let _ = writeln!(stderr, "{severity}[{}]: {}{at}", d.code, d.message);
            }
        }
    }
}
