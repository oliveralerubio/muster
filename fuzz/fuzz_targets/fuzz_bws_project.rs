#![no_main]
use libfuzzer_sys::fuzz_target;
use muster::providers::cli::validate_bws_project;

fuzz_target!(|data: &str| {
    if validate_bws_project(data).is_ok() {
        assert!(!data.starts_with('-') && data.len() <= 64);
        assert!(!data.chars().any(|c| c.is_whitespace() || c.is_control()));
    }
});
