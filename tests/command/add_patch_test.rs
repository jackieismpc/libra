//! Hidden `add -p` auto-advance session (HF-16 / M-PATCH).

use std::{fs, path::Path, process::Output};

use tempfile::tempdir;

use super::{
    assert_cli_success, configure_identity_via_cli, init_repo_via_cli, parse_cli_error_stderr,
    run_libra_command, run_libra_command_with_stdin, run_libra_command_with_stdin_and_env,
};

fn create_committed_repo() -> tempfile::TempDir {
    let repo = tempdir().expect("tempdir");
    init_repo_via_cli(repo.path());
    configure_identity_via_cli(repo.path());
    repo
}

fn numbered_lines(count: usize) -> String {
    (1..=count).map(|i| format!("line {i}\n")).collect()
}

fn write_numbered(path: &Path, count: usize) {
    fs::write(path, numbered_lines(count)).unwrap();
}

fn change_lines(path: &Path, count: usize, marks: &[(usize, &str)]) {
    let mut lines: Vec<String> = (1..=count).map(|i| format!("line {i}")).collect();
    for (n, text) in marks {
        lines[n - 1] = (*text).to_string();
    }
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn commit_file(repo: &Path, rel: &str, contents: &str, message: &str) {
    fs::write(repo.join(rel), contents).unwrap();
    assert_cli_success(
        &run_libra_command(&["add", rel], repo),
        &format!("add {rel}"),
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", message, "--no-verify"], repo),
        message,
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn cached_diff(repo: &Path) -> String {
    let output = run_libra_command(&["diff", "--cached"], repo);
    assert_cli_success(&output, "diff --cached");
    stdout(&output)
}

fn add_patch(repo: &Path, extra: &[&str], stdin_body: &str) -> Output {
    let mut args = vec!["add", "-p"];
    args.extend(extra);
    run_libra_command_with_stdin(&args, repo, stdin_body)
}

#[test]
fn test_add_patch_commands_matrix() {
    let repo = create_committed_repo();
    let root = repo.path();
    write_numbered(&root.join("first.txt"), 40);
    write_numbered(&root.join("second.txt"), 20);
    assert_cli_success(&run_libra_command(&["add", "."], root), "add baseline");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "base", "--no-verify"], root),
        "base",
    );
    change_lines(
        &root.join("first.txt"),
        40,
        &[(2, "first-A"), (16, "first-B")],
    );
    change_lines(
        &root.join("second.txt"),
        20,
        &[(2, "second-A"), (16, "second-B")],
    );

    // A1: help, multi-letter error, unknown command, y/n then q.
    let a1 = add_patch(root, &[], "?\nzz\nw\ny\nn\nq\n");
    assert_cli_success(&a1, "A1");
    let text = stdout(&a1);
    assert!(text.contains("y - stage this hunk"), "{text}");
    assert!(
        text.contains("Only one letter is expected, got 'zz'"),
        "{text}"
    );
    assert!(
        text.contains("Unknown command 'w' (use '?' for help)"),
        "{text}"
    );
    assert!(
        text.contains("diff --git a/first.txt b/first.txt"),
        "{text}"
    );
    assert!(
        text.contains("diff --git a/second.txt b/second.txt"),
        "{text}"
    );
    let cached = cached_diff(root);
    assert!(cached.contains("first-A"), "{cached}");
    assert!(!cached.contains("first-B"), "{cached}");
    assert!(!cached.contains("second-A"), "{cached}");

    // Reset index to HEAD so later cases start clean.
    assert_cli_success(&run_libra_command(&["reset"], root), "reset after A1");

    // A2: a then d.
    let a2 = add_patch(root, &[], "a\nd\n");
    assert_cli_success(&a2, "A2");
    let cached = cached_diff(root);
    assert!(
        cached.contains("first-A") && cached.contains("first-B"),
        "{cached}"
    );
    assert!(
        !cached.contains("second-A") && !cached.contains("second-B"),
        "{cached}"
    );
    assert_cli_success(&run_libra_command(&["reset"], root), "reset after A2");

    // A4: goto / search errors and a successful jump.
    let a4_err = add_patch(root, &["first.txt"], "g99\n/no-such\nq\n");
    assert_cli_success(&a4_err, "A4 errors");
    let text = stdout(&a4_err);
    assert!(text.contains("No other hunks to goto"), "{text}");
    assert!(text.contains("No other hunks to search"), "{text}");

    let a4_ok = add_patch(root, &["first.txt"], "g2\nq\n");
    assert_cli_success(&a4_ok, "A4 goto");
    let text = stdout(&a4_ok);
    assert!(text.contains("(2/2) Stage this hunk"), "{text}");
    assert_cli_success(&run_libra_command(&["reset"], root), "reset after A4");

    // A5: deletion + mode change + binary skip.
    fs::create_dir_all(root.join("kind")).unwrap();
    commit_file(root, "kind/delete-me.txt", "gone\n", "add delete-me");
    commit_file(root, "kind/mode.txt", "mode\n", "add mode");
    commit_file(root, "kind/keep.txt", "keep\n", "add keep");
    fs::remove_file(root.join("kind/delete-me.txt")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(root.join("kind/mode.txt")).unwrap();
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(root.join("kind/mode.txt"), perms).unwrap();
    }
    fs::write(root.join("kind/bin.dat"), b"a\0b").unwrap();
    assert_cli_success(
        &run_libra_command(&["add", "kind/bin.dat"], root),
        "track binary",
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "binary", "--no-verify"], root),
        "commit binary",
    );
    fs::write(root.join("kind/bin.dat"), b"a\0c").unwrap();

    let a5 = add_patch(root, &["kind"], "n\nn\n");
    assert_cli_success(&a5, "A5");
    let text = stdout(&a5);
    assert!(text.contains("Stage deletion"), "{text}");
    #[cfg(unix)]
    assert!(text.contains("Stage mode change"), "{text}");
    assert!(!text.contains("bin.dat"), "{text}");

    // A6: no changes / all-binary.
    let clean = add_patch(root, &["kind/keep.txt"], "");
    assert_cli_success(&clean, "A6 no changes");
    assert_eq!(stdout(&clean).trim(), "No changes.");

    let bin_only = add_patch(root, &["kind/bin.dat"], "");
    assert_cli_success(&bin_only, "A6 binary");
    assert_eq!(stdout(&bin_only).trim(), "Only binary files changed.");

    // A8: pathspec ignores untracked siblings.
    fs::create_dir_all(root.join("scope")).unwrap();
    commit_file(root, "scope/tracked.txt", "t0\n", "scope tracked");
    fs::write(root.join("scope/tracked.txt"), "t1\n").unwrap();
    fs::write(root.join("scope/untracked.txt"), "u\n").unwrap();
    fs::write(root.join("outside.txt"), "o\n").unwrap();
    let a8 = add_patch(root, &["scope"], "y\n");
    assert_cli_success(&a8, "A8");
    let text = stdout(&a8);
    assert!(text.contains("scope/tracked.txt"), "{text}");
    assert!(!text.contains("untracked.txt"), "{text}");
    assert!(!text.contains("outside.txt"), "{text}");

    // A9: q keeps only prior y.
    assert_cli_success(&run_libra_command(&["reset"], root), "reset before A9");
    change_lines(
        &root.join("first.txt"),
        40,
        &[(2, "first-A"), (16, "first-B")],
    );
    let a9 = add_patch(root, &["first.txt"], "y\nq\n");
    assert_cli_success(&a9, "A9");
    let cached = cached_diff(root);
    assert!(cached.contains("first-A"), "{cached}");
    assert!(!cached.contains("first-B"), "{cached}");
}

#[test]
fn test_t3701_roll_over_sequences() {
    let repo = create_committed_repo();
    let root = repo.path();
    write_numbered(&root.join("roll.txt"), 50);
    assert_cli_success(&run_libra_command(&["add", "roll.txt"], root), "add roll");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "roll", "--no-verify"], root),
        "commit roll",
    );
    change_lines(
        &root.join("roll.txt"),
        50,
        &[
            (2, "mark-1"),
            (14, "mark-2"),
            (26, "mark-3"),
            (38, "mark-4"),
        ],
    );

    let output = add_patch(root, &["roll.txt"], "g3\ny\nk\nq\n");
    assert_cli_success(&output, "A3 roll-over");
    let text = stdout(&output);
    assert!(text.contains("(1/4) Stage this hunk"), "{text}");
    assert!(text.contains("(3/4) Stage this hunk"), "{text}");
    assert!(text.contains("(4/4) Stage this hunk"), "{text}");
    assert!(text.contains("(2/4) Stage this hunk"), "{text}");
    let cached = cached_diff(root);
    assert!(cached.contains("mark-3"), "{cached}");
    assert!(!cached.contains("mark-1"), "{cached}");
    assert!(!cached.contains("mark-2"), "{cached}");
    assert!(!cached.contains("mark-4"), "{cached}");
}

#[test]
fn test_add_patch_eof_quits() {
    let repo = create_committed_repo();
    let root = repo.path();
    commit_file(root, "alpha.txt", "a0\n", "alpha");
    commit_file(root, "beta.txt", "b0\n", "beta");
    fs::write(root.join("alpha.txt"), "a1\n").unwrap();
    fs::write(root.join("beta.txt"), "b1\n").unwrap();

    let output = add_patch(root, &[], "");
    assert_cli_success(&output, "A7 eof");
    let text = stdout(&output);
    assert!(
        text.contains("diff --git a/alpha.txt b/alpha.txt"),
        "{text}"
    );
    assert!(
        !text.contains("diff --git a/beta.txt b/beta.txt"),
        "later file must not render on EOF: {text}"
    );
    assert!(cached_diff(root).trim().is_empty());
}

#[test]
fn test_add_patch_machine_modes_rejected() {
    let repo = create_committed_repo();
    let root = repo.path();
    commit_file(root, "m.txt", "m0\n", "m");
    fs::write(root.join("m.txt"), "m1\n").unwrap();

    let json = run_libra_command(&["--json", "add", "-p", "m.txt"], root);
    assert_eq!(json.status.code(), Some(129), "{}", stdout(&json));
    let (_, report) = parse_cli_error_stderr(&json.stderr);
    assert_eq!(report.error_code, "LBR-CLI-002");
    assert!(cached_diff(root).trim().is_empty());

    let dry = run_libra_command(&["add", "-p", "--dry-run", "m.txt"], root);
    assert_eq!(dry.status.code(), Some(129));
    let (_, report) = parse_cli_error_stderr(&dry.stderr);
    assert_eq!(report.error_code, "LBR-CLI-002");
    assert!(cached_diff(root).trim().is_empty());
    assert_eq!(fs::read_to_string(root.join("m.txt")).unwrap(), "m1\n");
}

#[test]
fn test_t3701_no_auto_advance_matrix() {
    let repo = create_committed_repo();
    let root = repo.path();
    commit_file(root, "first-file", "f0\n", "first");
    commit_file(root, "second-file", "s0\n", "second");
    fs::write(root.join("first-file"), "f1\n").unwrap();
    fs::write(root.join("second-file"), "s1\n").unwrap();

    // V1: > then q renders the second file.
    let v1 = run_libra_command_with_stdin(&["add", "-p", "--no-auto-advance"], root, ">\nq\n");
    assert_cli_success(&v1, "V1");
    let text = stdout(&v1);
    assert!(text.contains("b/second-file"), "{text}");
    assert!(cached_diff(root).trim().is_empty());

    // V2: n > < y q — second visit shows (was: n); first-file is staged.
    let v2 =
        run_libra_command_with_stdin(&["add", "-p", "--no-auto-advance"], root, "n\n>\n<\ny\nq\n");
    assert_cli_success(&v2, "V2");
    let text = stdout(&v2);
    assert!(text.contains("(was: n)"), "{text}");
    let cached = cached_diff(root);
    assert!(cached.contains("first-file"), "{cached}");
    assert!(!cached.contains("second-file"), "{cached}");
    assert_cli_success(&run_libra_command(&["reset"], root), "reset V2");

    // V3: y > < n q — (was: y); cached empty after overwrite.
    let v3 =
        run_libra_command_with_stdin(&["add", "-p", "--no-auto-advance"], root, "y\n>\n<\nn\nq\n");
    assert_cli_success(&v3, "V3");
    let text = stdout(&v3);
    assert!(text.contains("(was: y)"), "{text}");
    assert!(cached_diff(root).trim().is_empty(), "{}", cached_diff(root));

    // V4: only y then EOF stays and reapplies.
    commit_file(root, "stay", "old\n", "stay");
    fs::write(root.join("stay"), "new\n").unwrap();
    let v4 = run_libra_command_with_stdin(&["add", "-p", "--no-auto-advance", "stay"], root, "y\n");
    assert_cli_success(&v4, "V4");
    let text = stdout(&v4);
    assert!(text.contains("(1/1) Stage this hunk (was: y)"), "{text}");
    assert!(!text.contains("diff --git a/first-file"), "{text}");
    assert!(cached_diff(root).contains("stay"));
    assert_cli_success(&run_libra_command(&["reset"], root), "reset V4");

    // V5: help summary only when every hunk is decided.
    write_numbered(&root.join("sum.txt"), 40);
    assert_cli_success(&run_libra_command(&["add", "sum.txt"], root), "add sum");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "sum", "--no-verify"], root),
        "commit sum",
    );
    change_lines(
        &root.join("sum.txt"),
        40,
        &[(2, "s1"), (16, "s2"), (30, "s3")],
    );
    let v5_open = run_libra_command_with_stdin(
        &["add", "-p", "--no-auto-advance", "sum.txt"],
        root,
        "?\nq\n",
    );
    assert_cli_success(&v5_open, "V5 open");
    assert!(
        !stdout(&v5_open).contains("HUNKS SUMMARY"),
        "{}",
        stdout(&v5_open)
    );
    let v5_done = run_libra_command_with_stdin(
        &["add", "-p", "--no-auto-advance", "sum.txt"],
        root,
        "y\nJ\ny\nJ\nn\n?\nq\n",
    );
    assert_cli_success(&v5_done, "V5 done");
    assert!(
        stdout(&v5_done).contains("HUNKS SUMMARY - Hunks: 3, USE: 2, SKIP: 1"),
        "{}",
        stdout(&v5_done)
    );
    assert_cli_success(&run_libra_command(&["reset"], root), "reset V5");

    // V6: single file rejects >/< .
    let v6 = run_libra_command_with_stdin(
        &["add", "-p", "--no-auto-advance", "stay"],
        root,
        ">\n<\nq\n",
    );
    assert_cli_success(&v6, "V6");
    let text = stdout(&v6);
    assert!(text.contains("No next file"), "{text}");
    assert!(text.contains("No previous file"), "{text}");
    assert!(
        !text.contains("[y,n,q,a,d,>,<") && !text.contains(",>,<,"),
        "{text}"
    );

    // V8: last-wins --auto-advance --no-auto-advance stays; reverse pair advances.
    fs::write(root.join("stay"), "new2\n").unwrap();
    let stay = run_libra_command_with_stdin(
        &["add", "-p", "--auto-advance", "--no-auto-advance", "stay"],
        root,
        "y\nq\n",
    );
    assert_cli_success(&stay, "V8 no-auto last");
    assert!(stdout(&stay).contains("(was: y)"), "{}", stdout(&stay));
    assert_cli_success(&run_libra_command(&["reset"], root), "reset V8a");
    fs::write(root.join("stay"), "new2\n").unwrap();
    let advance = run_libra_command_with_stdin(
        &["add", "-p", "--no-auto-advance", "--auto-advance", "stay"],
        root,
        "y\n",
    );
    assert_cli_success(&advance, "V8 auto last");
    assert!(
        !stdout(&advance).contains("(was: y)"),
        "{}",
        stdout(&advance)
    );

    // V9: EOF applies the y decided under no-auto-advance.
    assert_cli_success(&run_libra_command(&["reset"], root), "reset V8b");
    fs::write(root.join("stay"), "new2\n").unwrap();
    let v9 = run_libra_command_with_stdin(&["add", "-p", "--no-auto-advance", "stay"], root, "y\n");
    assert_cli_success(&v9, "V9");
    assert!(cached_diff(root).contains("stay"));
}

#[test]
fn test_add_no_auto_advance_requires_patch() {
    let repo = create_committed_repo();
    let root = repo.path();
    commit_file(root, "a.txt", "a\n", "a");
    fs::write(root.join("a.txt"), "b\n").unwrap();
    let before = cached_diff(root);

    let missing = run_libra_command(&["add", "--no-auto-advance"], root);
    assert_eq!(missing.status.code(), Some(128));
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(
        err.contains("the option '--no-auto-advance' requires '--interactive/--patch'"),
        "{err}"
    );
    assert_eq!(cached_diff(root), before);

    let with_path = run_libra_command(&["add", "--no-auto-advance", "a.txt"], root);
    assert_eq!(with_path.status.code(), Some(128));
    assert_eq!(cached_diff(root), before);
}

#[test]
fn test_add_patch_split_matrix() {
    let repo = create_committed_repo();
    let root = repo.path();

    // L1: one hunk with three islands → Split into 3 hunks. then y n (auto).
    write_numbered(&root.join("s.txt"), 12);
    assert_cli_success(&run_libra_command(&["add", "s.txt"], root), "add s");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "s", "--no-verify"], root),
        "commit s",
    );
    change_lines(
        &root.join("s.txt"),
        12,
        &[(4, "isle-1"), (6, "isle-2"), (8, "isle-3")],
    );
    let l1 = add_patch(root, &["s.txt"], "s\ny\nn\nq\n");
    assert_cli_success(&l1, "L1");
    let text = stdout(&l1);
    assert!(text.contains("Split into 3 hunks."), "{text}");
    assert!(text.contains("@@ -"), "{text}");
    let cached = cached_diff(root);
    assert!(cached.contains("isle-1"), "{cached}");
    assert!(!cached.contains("isle-2"), "{cached}");
    assert_cli_success(&run_libra_command(&["reset"], root), "reset L1");

    // L3: unsplittable hunk.
    change_lines(&root.join("s.txt"), 12, &[(4, "only")]);
    let l3 = add_patch(root, &["s.txt"], "s\nq\n");
    assert_cli_success(&l3, "L3");
    assert!(
        stdout(&l3).contains("Sorry, cannot split this hunk"),
        "{}",
        stdout(&l3)
    );
    assert_cli_success(&run_libra_command(&["reset"], root), "reset L3");
    change_lines(&root.join("s.txt"), 12, &[(4, "line 4")]);

    // L4: split, decide, navigate, decisions stick.
    write_numbered(&root.join("nav-a.txt"), 10);
    write_numbered(&root.join("nav-b.txt"), 10);
    assert_cli_success(
        &run_libra_command(&["add", "nav-a.txt", "nav-b.txt"], root),
        "add nav",
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "nav", "--no-verify"], root),
        "commit nav",
    );
    change_lines(&root.join("nav-a.txt"), 10, &[(3, "A-keep"), (7, "A-drop")]);
    change_lines(&root.join("nav-b.txt"), 10, &[(3, "B-x")]);
    let l4 = run_libra_command_with_stdin(
        &["add", "-p", "--no-auto-advance"],
        root,
        "s\ny\nJ\nn\n>\n<\nq\n",
    );
    assert_cli_success(&l4, "L4");
    let text = stdout(&l4);
    assert!(text.contains("Split into 2 hunks."), "{text}");
    let cached = cached_diff(root);
    assert!(cached.contains("A-keep"), "{cached}");
    assert!(!cached.contains("A-drop"), "{cached}");
    assert!(!cached.contains("B-x"), "{cached}");
}

#[test]
fn test_t3701_selective_multi_file_tc0042() {
    let repo = create_committed_repo();
    let root = repo.path();
    for (name, _) in [("a.txt", "A"), ("b.txt", "B"), ("c.txt", "C")] {
        write_numbered(&root.join(name), 10);
    }
    assert_cli_success(&run_libra_command(&["add", "."], root), "add abc");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "abc", "--no-verify"], root),
        "commit abc",
    );
    change_lines(&root.join("a.txt"), 10, &[(2, "A2"), (5, "A5")]);
    change_lines(&root.join("b.txt"), 10, &[(3, "B3"), (8, "B8")]);
    change_lines(&root.join("c.txt"), 10, &[(1, "C1"), (6, "C6")]);

    let output = add_patch(root, &[], "s\ny\nn\ns\nn\ny\ns\ny\ny\n");
    assert_cli_success(&output, "TC-0042");
    let cached = cached_diff(root);
    assert!(cached.contains("+A2"), "{cached}");
    assert!(cached.contains("+B8"), "{cached}");
    assert!(cached.contains("+C1"), "{cached}");
    assert!(cached.contains("+C6"), "{cached}");
    assert!(!cached.contains("+A5"), "{cached}");
    assert!(!cached.contains("+B3"), "{cached}");
}

#[test]
fn test_add_patch_edit_hunk_matrix() {
    let repo = create_committed_repo();
    let root = repo.path();
    commit_file(root, "e.txt", "line 1\nline 2\nline 3\n", "e-base");
    fs::write(root.join("e.txt"), "line 1\nEDIT-ME\nline 3\n").unwrap();

    // E1: editor `true` keeps the hunk and stages it.
    let e1 = run_libra_command_with_stdin_and_env(
        &["add", "-p", "e.txt"],
        root,
        "e\n",
        &[("GIT_EDITOR", "true")],
    );
    assert_cli_success(&e1, "E1");
    assert!(
        cached_diff(root).contains("EDIT-ME"),
        "{}",
        cached_diff(root)
    );
    assert_cli_success(&run_libra_command(&["reset"], root), "reset E1");

    // E2: rewrite the `+` line; line numbers are recounted on parse.
    fs::write(root.join("e.txt"), "line 1\nEDIT-ME\nline 3\n").unwrap();
    let e2 = run_libra_command_with_stdin_and_env(
        &["add", "-p", "e.txt"],
        root,
        "e\n",
        &[(
            "GIT_EDITOR",
            "python3 -c \"import sys,pathlib; p=pathlib.Path(sys.argv[1]); p.write_text(p.read_text().replace('+EDIT-ME','+EDITED'))\"",
        )],
    );
    assert_cli_success(&e2, "E2");
    let cached = cached_diff(root);
    assert!(cached.contains("+EDITED"), "{cached}");
    assert!(!cached.contains("+EDIT-ME"), "{cached}");
    assert_cli_success(&run_libra_command(&["reset"], root), "reset E2");

    // E3: garbage edit, `n` discards.
    fs::write(root.join("e.txt"), "line 1\nEDIT-ME\nline 3\n").unwrap();
    let e3 = run_libra_command_with_stdin_and_env(
        &["add", "-p", "e.txt"],
        root,
        "e\nn\nq\n",
        &[(
            "GIT_EDITOR",
            "python3 -c \"import sys; open(sys.argv[1],'w').write('not a hunk\\n')\"",
        )],
    );
    assert_cli_success(&e3, "E3");
    assert!(
        stdout(&e3).contains(
            "Your edited hunk does not apply. Edit again (saying \"no\" discards!) [y/n]? "
        ),
        "{}",
        stdout(&e3)
    );
    assert!(
        !cached_diff(root).contains("EDIT-ME"),
        "{}",
        cached_diff(root)
    );

    // E4: emptying the buffer aborts the edit (hunk left unchanged).
    let e4 = run_libra_command_with_stdin_and_env(
        &["add", "-p", "e.txt"],
        root,
        "e\nq\n",
        &[(
            "GIT_EDITOR",
            "python3 -c \"import sys; open(sys.argv[1],'w').close()\"",
        )],
    );
    assert_cli_success(&e4, "E4");
    assert!(
        !cached_diff(root).contains("EDIT-ME"),
        "{}",
        cached_diff(root)
    );

    // E5: deletion / mode-change refuse `e`.
    commit_file(root, "gone.txt", "x\n", "gone");
    fs::remove_file(root.join("gone.txt")).unwrap();
    let e5 = add_patch(root, &["gone.txt"], "e\nq\n");
    assert_cli_success(&e5, "E5");
    assert!(
        stdout(&e5).contains("Sorry, cannot edit this hunk"),
        "{}",
        stdout(&e5)
    );

    // E6: buffer header is Git's manual-edit banner (unit-tested; CLI path
    // writes the same bytes to ADD_EDIT.patch before the editor runs).
    // E7: flags appear in `add --help`.
    let help = run_libra_command(&["add", "--help"], root);
    assert_cli_success(&help, "E7 help");
    let help_text = format!("{}{}", stdout(&help), String::from_utf8_lossy(&help.stderr));
    assert!(
        help_text.contains("-p") && help_text.contains("--patch"),
        "{help_text}"
    );
    assert!(help_text.contains("--auto-advance"), "{help_text}");
    assert!(help_text.contains("--no-auto-advance"), "{help_text}");
}

#[test]
fn test_add_patch_prompt_bytes_match_git() {
    let repo = create_committed_repo();
    let root = repo.path();
    commit_file(root, "first-file", "a\n", "f1");
    commit_file(root, "second-file", "b\n", "f2");
    fs::write(root.join("first-file"), "A\n").unwrap();
    fs::write(root.join("second-file"), "B\n").unwrap();

    let output = run_libra_command_with_stdin(&["add", "-p", "--no-auto-advance"], root, "?\nq\n");
    assert_cli_success(&output, "prompt bytes");
    let text = stdout(&output);
    assert!(
        text.contains("(1/1) Stage this hunk [y,n,q,a,d,e,>,<,p,P,?]? "),
        "{text}"
    );
    assert!(
        text.contains("e - manually edit the current hunk"),
        "{text}"
    );
    assert!(text.contains("y - stage this hunk"), "{text}");
}
