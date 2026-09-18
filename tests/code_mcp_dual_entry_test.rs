//! RC-16: `libra code --stdio` / dual MCP entry is no longer a public surface.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_libra"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", "/tmp")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .output()
        .expect("failed to spawn libra")
}

fn diag_of(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn libra_code_stdio_is_unknown() {
    let output = run(&["code", "--stdio"]);
    assert_ne!(output.status.code(), Some(0));
    let diag = diag_of(&output);
    assert!(
        diag.contains("not a libra command") || diag.contains("removed") || diag.contains("agent"),
        "libra code --stdio must be unknown: {diag}"
    );
}

#[test]
fn libra_code_cwd_stdio_is_unknown() {
    let output = run(&["code", "--stdio", "--cwd", "/tmp"]);
    assert_ne!(output.status.code(), Some(0));
    let diag = diag_of(&output);
    assert!(
        diag.contains("not a libra command") || diag.contains("removed") || diag.contains("agent"),
        "libra code --stdio --cwd must be unknown: {diag}"
    );
}
