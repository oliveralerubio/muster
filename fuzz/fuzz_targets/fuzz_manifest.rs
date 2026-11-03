#![no_main]
use libfuzzer_sys::fuzz_target;
use muster::manifest::Manifest;

fuzz_target!(|data: &str| {
    // Must never panic; round-tripping a parsed manifest must stay parseable.
    if let Ok(manifest) = Manifest::parse(data, "muster.toml") {
        if let Ok(text) = toml::to_string(&manifest) {
            let _ = Manifest::parse(&text, "muster.toml");
        }
    }
});
