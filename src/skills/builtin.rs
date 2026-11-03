//! Skills embedded in the binary. Trusted by construction: they bypass
//! `skills.lock` and are installed for the global target only.

use std::collections::BTreeMap;

const MUSTER_SKILL: &str = include_str!("../../assets/skills/muster/SKILL.md");

/// Built-in skills as `name -> relative path -> contents`.
pub fn builtin_skills() -> BTreeMap<String, BTreeMap<String, String>> {
    let mut skills = BTreeMap::new();
    skills.insert(
        "muster".to_string(),
        BTreeMap::from([("SKILL.md".to_string(), MUSTER_SKILL.to_string())]),
    );
    skills
}

/// Whether `name` is a built-in skill.
pub fn is_builtin(name: &str) -> bool {
    name == "muster"
}
