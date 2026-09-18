//! RC-16: `libra code --resume` is no longer a public surface.

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

#[test]
fn libra_code_resume_is_unknown() {
    let output = run(&["code", "--resume"]);
    assert_ne!(output.status.code(), Some(0));
    let diag = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diag.contains("not a libra command") || diag.contains("removed") || diag.contains("agent"),
        "libra code --resume must be unknown: {diag}"
    );
}
