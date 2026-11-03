#![no_main]
use libfuzzer_sys::fuzz_target;
use muster::skills::lock::SkillsLock;

fuzz_target!(|data: &str| {
    if let Ok(lock) = toml::from_str::<SkillsLock>(data) {
        // Serialization of anything that deserialized must not panic.
        let _ = toml::to_string_pretty(&lock);
    }
});
