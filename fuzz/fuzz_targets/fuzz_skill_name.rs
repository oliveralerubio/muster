#![no_main]
use libfuzzer_sys::fuzz_target;
use muster::commands::skill::validate_skill_name;
use muster::skills::lock::is_valid_name;

fuzz_target!(|data: &str| {
    let valid = is_valid_name(data);
    // Both validators must agree, and accepted names are a single safe component.
    assert_eq!(valid, validate_skill_name(data).is_ok());
    if valid {
        assert!(!data.contains(['/', '\\', '\0']) && !data.contains(".."));
        assert!(!data.starts_with('-') && !data.chars().any(char::is_control));
    }
});
