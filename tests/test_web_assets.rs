//! The web panel's JavaScript is regression-tested in Node (see
//! `tests/js/app.test.mjs`): Save gating on preview including stale-response
//! and payload double-check guards (QA round-2 finding 1), clearing preview
//! state when an edit opens (QA round-2 finding 2), and lossless JSON-array
//! list values (QA round-2 finding 3).

use std::path::PathBuf;
use std::process::Command;

#[test]
fn web_panel_javascript_passes_its_node_suite() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let harness = root.join("tests/js/app.test.mjs");
    let app_js = root.join("src/web/assets/app.js");
    let node = which_node().unwrap_or_else(|| {
        panic!(
            "node is required to test {} (the QA gate also runs `node --check` on it)",
            app_js.display()
        )
    });
    let output = Command::new(&node)
        .arg(&harness)
        .env("APP_JS", &app_js)
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("run the node test harness");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "node suite failed ({}):\n{stdout}{stderr}",
        output.status
    );
    assert!(
        stdout.contains("ok testSecretApprovalsRequireConfirmationAndRefresh")
            && stdout.contains("ok testProvidersViewListsReferencesAndAddsAndRemoves")
            && stdout.contains("ok testProfileSaveIsGatedOnPreview")
            && stdout.contains("ok testMcpFormUsesJsonArrayValues")
            && stdout.contains("ok testMcpPreviewIgnoresStaleResponse")
            && stdout.contains("ok testMcpEditClearsPreviewAndGuardsSave")
            && stdout.contains("ok testUsageShowsCoverageActiveAndUnusedItems")
            && stdout.contains("ok testUsageWithoutSessionsExplainsNoCoverage")
            && stdout.contains("ok testEffectiveViewShowsStatesAndEscapesNames")
            && stdout.contains("ok testDashboardSyncOperationsRedesign")
      && stdout.contains("ok testFleetTableColumnWidthsDeclaredOnce")
            && stdout.contains("ok testSkillsViewRendersUnmanagedSectionAndEscapesNames"),
        "the suite must actually run its tests:\n{stdout}{stderr}"
    );
}

/// Find `node` on PATH (integration tests may run with a stripped PATH).
fn which_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let ext = if cfg!(windows) { ".exe" } else { "" };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(format!("node{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
