//! RC-10: `review --fix` and `investigate fix` are gone. Clap must reject
//! them as unknown surfaces; they must not emit `LBR-AGENT-010` or talk
//! to `/api/code`.

use super::run_libra_command;

fn assert_unknown_surface(out: &std::process::Output, context: &str) {
    assert_ne!(
        out.status.code(),
        Some(0),
        "{context} must not succeed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("LBR-AGENT-010"),
        "{context} must not emit LBR-AGENT-010: {stderr}"
    );
    assert!(
        !stderr.contains("/api/code"),
        "{context} must not mention /api/code: {stderr}"
    );
}

#[test]
fn review_fix_flag_is_unknown() {
    let out = run_libra_command(&["review", "--fix"], std::path::Path::new("."));
    assert_unknown_surface(&out, "review --fix");
}

#[test]
fn investigate_fix_subcommand_is_unknown() {
    let out = run_libra_command(
        &["investigate", "fix", "some-run-id"],
        std::path::Path::new("."),
    );
    assert_unknown_surface(&out, "investigate fix");
}
