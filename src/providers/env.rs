//! `env://VAR`: a variable of Muster's own process environment.

use super::{Description, Provider, ProviderError, ProviderErrorKind};
use crate::errors::MusterError;
use crate::secrets::{Scheme, SecretRef, SecretValue};

pub struct Env;

impl Provider for Env {
    fn describe(&self) -> Description {
        Description {
            scheme: Scheme::Env,
            name: "Environment variables",
            builtin: true,
            binary: None,
        }
    }

    fn check(&mut self) -> Result<(), MusterError> {
        Ok(())
    }

    fn resolve(&mut self, reference: &SecretRef) -> Result<SecretValue, MusterError> {
        let var = &reference.segments()[0];
        match std::env::var(var) {
            Ok(value) => Ok(SecretValue::new(value)),
            Err(std::env::VarError::NotPresent) => Err(ProviderError::new(
                ProviderErrorKind::NotFound,
                format!("environment variable {var} is not set ({reference})"),
            )
            .into()),
            Err(std::env::VarError::NotUnicode(_)) => Err(ProviderError::new(
                ProviderErrorKind::InvalidReference,
                format!("environment variable {var} is not valid UTF-8 ({reference})"),
            )
            .into()),
        }
    }
}
