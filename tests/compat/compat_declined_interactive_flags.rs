//! HF-14 / ADR-HF-14: remaining D15/D16 interactive flags fail closed.
//!
//! Guards:
//! - every `DECLINED_INTERACTIVE_FLAGS` row has a D15/D16 anchor
//! - `_compatibility.md` still defines those anchors
//! - each row has an integration spawn covering M-DECLINED X1–X5 / X7
//! - table-miss unknown flags stay `LBR-CLI-002` (X6)
//! - `--help` does not advertise the declined flags

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use libra::cli::DECLINED_INTERACTIVE_FLAGS;
use tempfile::tempdir;

fn libra() -> &'static str {
    env!("CARGO_BIN_EXE_libra")
}

fn run_at(args: &[&str], cwd: &Path) -> Output {
    let home = cwd.join(".compat-home");
    fs::create_dir_all(&home).unwrap();
    Command::new(libra())
        .args(args)
        .current_dir(cwd)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("RUST_LOG")
        .env_remove("LIBRA_LOG")
        .output()
        .unwrap()
}

fn run(args: &[&str]) -> Output {
    Command::new(libra())
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", "/tmp")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .output()
        .unwrap()
}

fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    let out = run_at(&["init"], repo);
    assert!(out.status.success(), "init: {:?}", out);
}

fn argv_for(entry: &libra::cli::DeclinedInteractiveFlag) -> Vec<&'static str> {
    let mut args = vec![entry.command];
    if let Some(nested) = entry.nested {
        args.push(nested);
    }
    args.push(entry.flags[0]);
    args
}

#[test]
fn declined_table_anchors_match_compatibility_docs() {
    let compat = include_str!("../../docs/development/commands/_compatibility.md");
    assert!(compat.contains("### D15：跨命令 patch mode"));
    assert!(compat.contains("### D16：交互式 rebase 和 todo 编辑"));
    assert!(
        DECLINED_INTERACTIVE_FLAGS
            .iter()
            .all(|entry| entry.docs_anchor == "D15" || entry.docs_anchor == "D16")
    );
    assert!(!DECLINED_INTERACTIVE_FLAGS.is_empty());
}

#[test]
fn declined_flags_are_unsupported_and_write_nothing() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_repo(&repo);
    fs::write(repo.join("tracked.txt"), "v1\n").unwrap();
    assert!(run_at(&["add", "tracked.txt"], &repo).status.success());
    assert!(
        run_at(&["config", "user.name", "Tester"], &repo)
            .status
            .success()
    );
    assert!(
        run_at(&["config", "user.email", "t@example.com"], &repo)
            .status
            .success()
    );
    assert!(
        run_at(&["commit", "-m", "initial", "--no-verify"], &repo)
            .status
            .success()
    );
    fs::write(repo.join("tracked.txt"), "v2\n").unwrap();
    let before = run_at(&["status", "--porcelain"], &repo);
    let before_out = before.stdout.clone();

    for entry in DECLINED_INTERACTIVE_FLAGS {
        let args = argv_for(entry);
        let output = run_at(&args, &repo);
        assert_eq!(
            output.status.code(),
            Some(128),
            "{args:?} exit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("LBR-UNSUPPORTED-001"),
            "{args:?} code: {stderr}"
        );
        assert!(stderr.contains(entry.message), "{args:?} message: {stderr}");

        let json_args: Vec<&str> = std::iter::once("--json")
            .chain(args.iter().copied())
            .collect();
        let json = run_at(&json_args, &repo);
        assert_eq!(json.status.code(), Some(128), "{json_args:?}");
        let jerr = String::from_utf8_lossy(&json.stderr);
        assert!(
            jerr.contains("LBR-UNSUPPORTED-001"),
            "{json_args:?} envelope: {jerr}"
        );

        let after = run_at(&["status", "--porcelain"], &repo);
        assert_eq!(after.stdout, before_out, "{args:?} must write nothing");
    }
}

#[test]
fn table_miss_unknown_flag_stays_cli_invalid() {
    let output = run(&["add", "--bogus"]);
    assert_eq!(output.status.code(), Some(129));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("LBR-CLI-002"), "{stderr}");
    assert!(!stderr.contains("LBR-UNSUPPORTED-001"), "{stderr}");
}

#[test]
fn help_does_not_advertise_declined_flags() {
    for command in ["add", "commit", "restore", "checkout", "stash", "rebase"] {
        let output = run(&[command, "--help"]);
        assert!(
            output.status.success(),
            "{command} --help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if command != "add" && command != "reset" {
            assert!(
                !text.contains("--auto-advance"),
                "{command} --help must not advertise --auto-advance"
            );
        }
        if command != "stash" && command != "add" && command != "reset" {
            assert!(
                !text.contains("--patch") && !text.contains(" -p,"),
                "{command} --help must not advertise -p/--patch: {text}"
            );
        }
        // HF-24 / ADR-HF-19 §7: rebase -i/--interactive is a public surface.
        // add -i remains declined (D15). --rebase-merges stays declined (D16).
        if command != "rebase" {
            assert!(
                !text.contains("--interactive") && !text.contains(" -i,"),
                "{command} --help must not advertise -i/--interactive"
            );
        } else {
            assert!(
                text.contains("--interactive") && text.contains(" -i,"),
                "rebase --help must advertise -i/--interactive after HF-24"
            );
        }
        assert!(
            !text.contains("--rebase-merges"),
            "{command} --help must not advertise --rebase-merges"
        );
    }
}
