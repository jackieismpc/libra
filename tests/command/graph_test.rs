//! RC-12: top-level `libra graph` is gone. Clap must reject it as an
//! unknown command; the capture graph stays on `libra --json agent graph`.

use super::run_libra_command;

fn assert_unknown_graph(output: &std::process::Output, context: &str) {
    assert_ne!(
        output.status.code(),
        Some(0),
        "{context} must not succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("LBR-CLI-002") || !stderr.contains("thread_id UUID"),
        "{context} must not take the old Code-thread graph path: {stderr}"
    );
    assert!(
        stderr.contains("graph")
            || stderr.contains("unrecognized")
            || stderr.contains("unexpected")
            || stderr.contains("error:"),
        "{context} should reject the unknown command: {stderr}"
    );
}

#[test]
fn top_level_graph_is_unknown() {
    let output = run_libra_command(&["graph"], std::path::Path::new("."));
    assert_unknown_graph(&output, "libra graph");
}

#[test]
fn top_level_graph_json_is_unknown() {
    let output = run_libra_command(
        &["graph", "--json", "11111111-1111-4111-8111-111111111111"],
        std::path::Path::new("."),
    );
    assert_unknown_graph(&output, "libra graph --json");
}

#[test]
fn top_level_graph_help_is_unknown() {
    let output = run_libra_command(&["graph", "--help"], std::path::Path::new("."));
    assert_unknown_graph(&output, "libra graph --help");
}
