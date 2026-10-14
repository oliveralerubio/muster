use crate::output::Diagnostic;

/// Every failure a command can report. The variant decides the exit code.
#[derive(Debug, thiserror::Error)]
pub enum MusterError {
    #[error("{0}")]
    Operational(String),
    #[error("{0}")]
    Usage(String),
    #[error("{message}")]
    InvalidConfig {
        message: String,
        path: Option<String>,
    },
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Security(String),
    #[error("interrupted")]
    Interrupted,
}

impl MusterError {
    /// Exit code from the spec table.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Operational(_) => 1,
            Self::Usage(_) => 2,
            Self::InvalidConfig { .. } => 3,
            Self::Unavailable(_) => 4,
            Self::Security(_) => 5,
            Self::Interrupted => 130,
        }
    }

    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Operational(_) => "operational",
            Self::Usage(_) => "usage",
            Self::InvalidConfig { .. } => "invalid_config",
            Self::Unavailable(_) => "unavailable",
            Self::Security(_) => "security",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn to_diagnostic(&self) -> Diagnostic {
        let diag = Diagnostic::error(self.code(), self.to_string());
        match self {
            Self::InvalidConfig {
                path: Some(path), ..
            } => diag.with_path(path.clone()),
            _ => diag,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MusterError::*;

    #[test]
    fn exit_codes_match_the_spec_table() {
        let cases = [
            (Operational("x".into()), 1),
            (Usage("x".into()), 2),
            (
                InvalidConfig {
                    message: "x".into(),
                    path: None,
                },
                3,
            ),
            (Unavailable("x".into()), 4),
            (Security("x".into()), 5),
            (Interrupted, 130),
        ];
        for (err, code) in cases {
            assert_eq!(err.exit_code(), code, "{err:?}");
        }
    }
}
