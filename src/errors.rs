use crate::output::Diagnostic;
use crate::providers::ProviderError;

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
    /// The Cloud accepted another publication first (`409 revision_conflict`):
    /// this machine's bundle is retained there as a conflict proposal and its
    /// local history is untouched.
    #[error("{0}")]
    Conflict(String),
    /// Cloud answered `429 rate_limited` and the request was not applied
    /// (after the one allowed retry, or because it is not safe to repeat).
    #[error("{0}")]
    RateLimited(String),
    #[error("interrupted")]
    Interrupted,
    /// A typed secret-provider failure; its kind decides code and exit code.
    #[error(transparent)]
    Provider(#[from] ProviderError),
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
            Self::Conflict(_) => 1,
            Self::RateLimited(_) => 1,
            Self::Interrupted => 130,
            Self::Provider(e) => e.exit_code(),
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
            Self::Conflict(_) => "revision_conflict",
            Self::RateLimited(_) => "cloud_rate_limited",
            Self::Interrupted => "interrupted",
            Self::Provider(e) => e.code(),
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
            (Conflict("x".into()), 1),
            (RateLimited("x".into()), 1),
            (Interrupted, 130),
        ];
        for (err, code) in cases {
            assert_eq!(err.exit_code(), code, "{err:?}");
        }
    }
}
