//! Secret values that never print their plaintext.

use std::fmt;

use secrecy::{ExposeSecret, SecretString};

/// A plaintext secret. `Debug` and `Display` always render `<redacted>`;
/// only [`SecretValue::expose`] yields the text.
#[derive(Clone)]
pub struct SecretValue(SecretString);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(SecretString::from(value))
    }

    /// The plaintext. Call it at the last possible moment.
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}
