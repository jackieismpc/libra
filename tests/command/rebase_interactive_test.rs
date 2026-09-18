//! `rebase -i` todo generate / parse / replay and combination flags
//! (HF-21 / HF-28 / HF-22 / HF-23 / HF-24).

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Output};

use tempfile::tempdir;

use super::{
    assert_cli_success, configure_identity_via_cli, init_repo_via_cli, parse_cli_error_stderr,
    run_libra_command, run_libra_command_with_env,
};

fn create_repo() -> tempfile::TempDir {
    let repo = tempdir().expect("tempdir");
    init_repo_via_cli(repo.path());
    configure_identity_via_cli(repo.path());
    repo
}

fn commit_file(repo: &Path, rel: &str, body: &str, message: &str) {
    fs::write(repo.join(rel), format!("{body}\n")).expect("write");
    assert_cli_success(
        &run_libra_command(&["add", "--", rel], repo),
        &format!("add {rel}"),
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", message, "--no-verify"], repo),
        message,
    );
}

fn rev_parse(repo: &Path, rev: &str) -> String {
    let output = run_libra_command(&["rev-parse", rev], repo);
    assert_cli_success(&output, &format!("rev-parse {rev}"));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn abbrev7(oid: &str) -> String {
    oid.chars().take(7).collect()
}

fn ls_files(repo: &Path) -> String {
    let output = run_libra_command(&["ls-files", "-s"], repo);
    assert_cli_success(&output, "ls-files");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn log_body(repo: &Path, rev: &str) -> String {
    let output = run_libra_command(&["log", "-1", "--format=%B", rev], repo);
    assert_cli_success(&output, &format!("log body {rev}"));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn log_subjects(repo: &Path) -> Vec<String> {
    let output = run_libra_command(&["log", "--oneline"], repo);
    assert_cli_success(&output, "log --oneline");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            line.split_once(' ')
                .map(|(_, msg)| msg)
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect()
}

fn current_branch(repo: &Path) -> String {
    let output = run_libra_command(&["branch", "--show-current"], repo);
    assert_cli_success(&output, "branch --show-current");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_script(repo: &Path, name: &str, body: &str) -> String {
    let script = repo.join(name);
    fs::write(&script, body).expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    script.display().to_string()
}

fn write_capture_editor(repo: &Path, dest: &Path) -> String {
    write_script(
        repo,
        ".capture-todo.sh",
        &format!("#!/bin/sh\ncp \"$1\" '{}'\n", dest.display()),
    )
}

fn write_capture_then_empty_editor(repo: &Path, dest: &Path) -> String {
    write_script(
        repo,
        ".capture-empty-todo.sh",
        &format!(
            "#!/bin/sh\ncp \"$1\" '{}'\nprintf '# emptied\\n' > \"$1\"\n",
            dest.display()
        ),
    )
}

fn write_todo_editor(repo: &Path, name: &str, body: &str) -> String {
    let todo = repo.join(format!(".{name}.todo"));
    fs::write(&todo, body).expect("write replacement todo");
    write_script(
        repo,
        &format!(".{name}.sh"),
        &format!("#!/bin/sh\ncp '{}' \"$1\"\n", todo.display()),
    )
}

fn rebase_i(repo: &Path, extra_env: &[(&str, &str)]) -> Output {
    run_libra_command_with_env(&["rebase", "-i", "HEAD~3"], repo, extra_env)
}

fn rebase_i_onto(repo: &Path, upstream: &str, extra_env: &[(&str, &str)]) -> Output {
    run_libra_command_with_env(&["rebase", "-i", upstream], repo, extra_env)
}

fn assert_zero_writes(repo: &Path, head: &str, index: &str) {
    assert_eq!(rev_parse(repo, "HEAD"), head, "HEAD must not move");
    assert_eq!(ls_files(repo), index, "index must not change");
    assert!(
        !repo.join(".libra").join("rebase-aux.json").exists(),
        "rebase-aux.json must not be written"
    );
    assert!(
        !repo.join(".libra").join("rebase-merge").exists(),
        "rebase-merge must not remain"
    );
    let abort = run_libra_command(&["rebase", "--abort"], repo);
    assert_eq!(abort.status.code(), Some(128), "no in-progress rebase");
    let (_, report) = parse_cli_error_stderr(&abort.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
}

fn setup_i1(repo: &Path) -> (String, String, String, String, String) {
    commit_file(repo, "a.txt", "init", "init");
    commit_file(repo, "a.txt", "A", "A");
    commit_file(repo, "a.txt", "B", "B");
    commit_file(repo, "a.txt", "C", "C");
    let onto = rev_parse(repo, "HEAD~3");
    let a = rev_parse(repo, "HEAD~2");
    let b = rev_parse(repo, "HEAD~1");
    let head = rev_parse(repo, "HEAD");
    let index = ls_files(repo);
    (onto, a, b, head, index)
}

#[test]
fn test_rebase_i_todo_generation_matrix() {
    let repo = create_repo();
    let root = repo.path();
    let (onto, a, b, head, index) = setup_i1(root);

    // T1: capture first-generate todo bytes, then replay unchanged (T2).
    let captured = root.join("captured.todo");
    let editor = write_capture_editor(root, &captured);
    let t1 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        t1.status.success(),
        "T1/T2 unchanged todo must succeed: {}",
        String::from_utf8_lossy(&t1.stderr)
    );
    let expected = libra::command::rebase_todo::render_todo(
        &[
            libra::command::rebase_todo::TodoRenderCommit {
                abbrev: abbrev7(&a),
                subject: "A".into(),
            },
            libra::command::rebase_todo::TodoRenderCommit {
                abbrev: abbrev7(&b),
                subject: "B".into(),
            },
            libra::command::rebase_todo::TodoRenderCommit {
                abbrev: abbrev7(&head),
                subject: "C".into(),
            },
        ],
        &abbrev7(&onto),
        &abbrev7(&head),
    );
    let got = fs::read_to_string(&captured).expect("captured todo");
    assert_eq!(
        got, expected,
        "T1 captured todo must match Git first-generate text"
    );
    assert_eq!(rev_parse(root, "HEAD"), head, "T2 hashes stay (HEAD)");
    assert_eq!(rev_parse(root, "HEAD~1"), b, "T2 hashes stay (B)");
    assert_eq!(rev_parse(root, "HEAD~2"), a, "T2 hashes stay (A)");
    assert_eq!(rev_parse(root, "HEAD~3"), onto, "T2 hashes stay (onto)");

    // T6: no editor configured → error and zero writes.
    let t6_none = rebase_i(root, &[]);
    assert_eq!(t6_none.status.code(), Some(128), "T6 unset editor");
    let (stderr, _) = parse_cli_error_stderr(&t6_none.stderr);
    assert!(
        stderr.contains("no sequence editor configured"),
        "T6 unset: {stderr}"
    );
    assert_zero_writes(root, &head, &index);

    // T6: GIT_SEQUENCE_EDITOR wins over GIT_EDITOR / EDITOR.
    let seq_wins = root.join("seq-wins.todo");
    let seq_editor = write_capture_then_empty_editor(root, &seq_wins);
    let t6_seq = rebase_i(
        root,
        &[
            ("GIT_SEQUENCE_EDITOR", seq_editor.as_str()),
            ("GIT_EDITOR", "false"),
            ("EDITOR", "false"),
        ],
    );
    assert_eq!(t6_seq.status.code(), Some(128));
    let (stderr, report) = parse_cli_error_stderr(&t6_seq.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
    assert!(stderr.contains("nothing to do"), "T6 seq: {stderr}");
    assert!(seq_wins.exists(), "GIT_SEQUENCE_EDITOR must run");
    assert_zero_writes(root, &head, &index);

    // T6: sequence.editor beats GIT_EDITOR.
    let cfg_capture = root.join("cfg.todo");
    let cfg_editor = write_capture_then_empty_editor(root, &cfg_capture);
    assert_cli_success(
        &run_libra_command(&["config", "set", "sequence.editor", &cfg_editor], root),
        "config set sequence.editor",
    );
    let t6_cfg = rebase_i(root, &[("GIT_EDITOR", "false"), ("EDITOR", "false")]);
    assert_eq!(t6_cfg.status.code(), Some(128));
    let (stderr, report) = parse_cli_error_stderr(&t6_cfg.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
    assert!(stderr.contains("nothing to do"), "T6 cfg: {stderr}");
    assert!(cfg_capture.exists(), "sequence.editor must run");
    assert_zero_writes(root, &head, &index);

    // T6: GIT_SEQUENCE_EDITOR still beats sequence.editor.
    let env_beats_cfg = root.join("env-beats-cfg.todo");
    let env_editor = write_capture_then_empty_editor(root, &env_beats_cfg);
    let t6_env = rebase_i(
        root,
        &[
            ("GIT_SEQUENCE_EDITOR", env_editor.as_str()),
            ("GIT_EDITOR", "false"),
        ],
    );
    assert_eq!(t6_env.status.code(), Some(128));
    assert!(env_beats_cfg.exists());
    assert_zero_writes(root, &head, &index);

    // T7: editor non-zero → abort, zero writes.
    let t7 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", "false")]);
    assert_eq!(t7.status.code(), Some(128), "T7");
    let (stderr, report) = parse_cli_error_stderr(&t7.stderr);
    assert_ne!(report.error_code, "LBR-UNSUPPORTED-001");
    assert!(
        stderr.contains("exited abnormally") || stderr.contains("edit aborted"),
        "T7: {stderr}"
    );
    assert_zero_writes(root, &head, &index);
}

#[test]
fn test_rebase_i_replay_lifecycle_matrix() {
    // T2: unchanged todo succeeds and keeps hashes.
    let repo = create_repo();
    let root = repo.path();
    let (onto, a, b, head, _index) = setup_i1(root);
    let t2 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", "true")]);
    assert!(
        t2.status.success(),
        "T2: {}",
        String::from_utf8_lossy(&t2.stderr)
    );
    assert_eq!(rev_parse(root, "HEAD"), head);
    assert_eq!(rev_parse(root, "HEAD~1"), b);
    assert_eq!(rev_parse(root, "HEAD~2"), a);
    assert_eq!(rev_parse(root, "HEAD~3"), onto);
    assert_eq!(current_branch(root).as_str(), "main");
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "T2 must clear aux"
    );

    // T3 uses independent files so reorder/drop do not conflict (t3404 style).
    let setup_t3 = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "a.txt", "A", "A");
        commit_file(&root, "b.txt", "B", "B");
        commit_file(&root, "c.txt", "C", "C");
        let a = rev_parse(&root, "HEAD~2");
        let b = rev_parse(&root, "HEAD~1");
        let c = rev_parse(&root, "HEAD");
        (repo, a, b, c)
    };

    // T3 / I2: reorder and delete B → HEAD-first log A, C, init.
    let (repo, a, _b, head) = setup_t3();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "reorder-delete",
        &format!("pick {} # C\npick {} # A\n", abbrev7(&head), abbrev7(&a)),
    );
    let t3_delete = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        t3_delete.status.success(),
        "T3 delete: {}",
        String::from_utf8_lossy(&t3_delete.stderr)
    );
    assert_eq!(log_subjects(root), vec!["A", "C", "init"]);

    // T3 / I3: abbreviations p/d delete B, keep A then C → C, A, init.
    let (repo, a, b, head) = setup_t3();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "abbrev-drop",
        &format!(
            "p {} # A\nd {} # B\np {} # C\n",
            abbrev7(&a),
            abbrev7(&b),
            abbrev7(&head)
        ),
    );
    let t3_abbrev = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        t3_abbrev.status.success(),
        "T3 abbrev: {}",
        String::from_utf8_lossy(&t3_abbrev.stderr)
    );
    assert_eq!(log_subjects(root), vec!["C", "A", "init"]);

    // T4: empty todo → nothing to do, no state.
    let repo = create_repo();
    let root = repo.path();
    let (_onto, _a, _b, head, index) = setup_i1(root);
    let editor = write_todo_editor(root, "empty", "# emptied\n");
    let t4 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert_eq!(t4.status.code(), Some(128), "T4");
    let (stderr, report) = parse_cli_error_stderr(&t4.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
    assert!(stderr.contains("nothing to do"), "T4: {stderr}");
    assert_zero_writes(root, &head, &index);

    // T5: invalid command creates in-progress at onto; --abort restores.
    let repo = create_repo();
    let root = repo.path();
    let (onto, _a, _b, head, _index) = setup_i1(root);
    let editor = write_todo_editor(root, "invalid", "frobnicate abcdef0\n");
    let t5 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert_eq!(t5.status.code(), Some(128), "T5");
    let (stderr, report) = parse_cli_error_stderr(&t5.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
    assert!(stderr.contains("invalid command 'frobnicate'"), "{stderr}");
    assert!(stderr.contains("invalid line"), "{stderr}");
    assert!(
        stderr.contains("--edit-todo") || report.hints.iter().any(|h| h.contains("--edit-todo")),
        "{stderr:?} {:?}",
        report.hints
    );
    assert_eq!(rev_parse(root, "HEAD"), onto, "T5 HEAD at onto");
    let detached = current_branch(root);
    assert!(
        detached.is_empty() || detached.contains("detached") || detached.contains("HEAD"),
        "T5 must detach: {detached}"
    );
    let abort = run_libra_command(&["rebase", "--abort"], root);
    assert!(
        abort.status.success(),
        "T5 abort: {}",
        String::from_utf8_lossy(&abort.stderr)
    );
    assert_eq!(rev_parse(root, "HEAD"), head);
    assert_eq!(current_branch(root).as_str(), "main");
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "T5 abort clears aux"
    );

    // T13 / T14: conflict stop → continue / abort.
    let conflict_setup = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "conflict.txt", "base", "Base");
        assert_cli_success(
            &run_libra_command(&["switch", "-c", "feature"], &root),
            "create feature",
        );
        commit_file(
            &root,
            "conflict.txt",
            "feature",
            "Feature modifies conflict.txt",
        );
        assert_cli_success(
            &run_libra_command(&["switch", "main"], &root),
            "switch main",
        );
        commit_file(&root, "conflict.txt", "main", "Main modifies conflict.txt");
        assert_cli_success(
            &run_libra_command(&["switch", "feature"], &root),
            "switch feature",
        );
        let orig = rev_parse(&root, "HEAD");
        (repo, orig)
    };

    let (repo, _orig) = conflict_setup();
    let root = repo.path();
    let t13 = rebase_i_onto(root, "main", &[("GIT_SEQUENCE_EDITOR", "true")]);
    assert_eq!(t13.status.code(), Some(128), "T13 conflict");
    let conflicted = fs::read_to_string(root.join("conflict.txt")).expect("conflict file");
    assert!(conflicted.contains("<<<<<<<"), "T13 markers: {conflicted}");
    fs::write(root.join("conflict.txt"), "resolved\n").expect("resolve");
    assert_cli_success(
        &run_libra_command(&["add", "conflict.txt"], root),
        "stage resolution",
    );
    let cont = run_libra_command(&["rebase", "--continue"], root);
    assert!(
        cont.status.success(),
        "T13 continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(
        log_subjects(root),
        vec![
            "Feature modifies conflict.txt",
            "Main modifies conflict.txt",
            "Base"
        ]
    );
    assert_eq!(current_branch(root).as_str(), "feature");
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "T13 clears aux"
    );

    let (repo, orig) = conflict_setup();
    let root = repo.path();
    let t14 = rebase_i_onto(root, "main", &[("GIT_SEQUENCE_EDITOR", "true")]);
    assert_eq!(t14.status.code(), Some(128), "T14 conflict");
    let abort = run_libra_command(&["rebase", "--abort"], root);
    assert!(
        abort.status.success(),
        "T14 abort: {}",
        String::from_utf8_lossy(&abort.stderr)
    );
    assert_eq!(rev_parse(root, "HEAD"), orig);
    assert_eq!(current_branch(root).as_str(), "feature");
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "T14 clears aux"
    );
}

#[test]
fn test_t3404_exchange_nop_drop() {
    let setup = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "one.txt", "one", "one");
        commit_file(&root, "two.txt", "two", "two");
        commit_file(&root, "three.txt", "three", "three");
        let one = rev_parse(&root, "HEAD~2");
        let two = rev_parse(&root, "HEAD~1");
        let three = rev_parse(&root, "HEAD");
        (repo, one, two, three)
    };

    // t3404:225 — no changes are a nop.
    let (repo, one, two, three) = setup();
    let root = repo.path();
    let nop = rebase_i_onto(root, "HEAD~2", &[("GIT_SEQUENCE_EDITOR", "true")]);
    assert!(
        nop.status.success(),
        "t3404 nop: {}",
        String::from_utf8_lossy(&nop.stderr)
    );
    assert_eq!(rev_parse(root, "HEAD"), three);
    assert_eq!(rev_parse(root, "HEAD~1"), two);
    assert_eq!(rev_parse(root, "HEAD~2"), one);

    // t3404:271 — exchange two commits.
    let (repo, _one, two, three) = setup();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "exchange",
        &format!(
            "pick {} # three\npick {} # two\n",
            abbrev7(&three),
            abbrev7(&two)
        ),
    );
    let exchange = rebase_i_onto(root, "HEAD~2", &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        exchange.status.success(),
        "t3404 exchange: {}",
        String::from_utf8_lossy(&exchange.stderr)
    );
    assert_eq!(log_subjects(root), vec!["two", "three", "one", "init"]);

    // t3404:1493 — drop.
    let (repo, _one, two, three) = setup();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "drop",
        &format!(
            "drop {} # two\npick {} # three\n",
            abbrev7(&two),
            abbrev7(&three)
        ),
    );
    let drop = rebase_i_onto(root, "HEAD~2", &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        drop.status.success(),
        "t3404 drop: {}",
        String::from_utf8_lossy(&drop.stderr)
    );
    assert_eq!(log_subjects(root), vec!["three", "one", "init"]);
}

#[test]
fn test_rebase_i_message_commands_matrix() {
    let setup_abc = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "a.txt", "A", "A");
        commit_file(&root, "b.txt", "B", "B");
        commit_file(&root, "c.txt", "C", "C");
        let a = rev_parse(&root, "HEAD~2");
        let b = rev_parse(&root, "HEAD~1");
        let c = rev_parse(&root, "HEAD");
        (repo, a, b, c)
    };

    // G1 / I4: squash + fixup → one commit, message A then B.
    let (repo, a, b, c) = setup_abc();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "g1",
        &format!(
            "pick {} # A\nsquash {} # B\nfixup {} # C\n",
            abbrev7(&a),
            abbrev7(&b),
            abbrev7(&c)
        ),
    );
    let g1 = rebase_i(
        root,
        &[
            ("GIT_SEQUENCE_EDITOR", editor.as_str()),
            ("GIT_EDITOR", "true"),
        ],
    );
    assert!(
        g1.status.success(),
        "G1: {}",
        String::from_utf8_lossy(&g1.stderr)
    );
    assert_eq!(log_subjects(root), vec!["A", "init"]);
    let body = log_body(root, "HEAD");
    let clean = body.split("gpgsig").next().unwrap_or(body.as_str());
    assert!(clean.contains('A'), "G1 body A: {clean}");
    assert!(
        clean.lines().any(|line| line.trim() == "B"),
        "G1 body B: {clean}"
    );
    assert!(
        !clean.lines().any(|line| line.trim() == "C"),
        "G1 fixup message discarded: {clean}"
    );
    let author = run_libra_command(&["log", "-1", "--format=%an"], root);
    assert_cli_success(&author, "G6 author");
    assert_eq!(
        String::from_utf8_lossy(&author.stdout).trim(),
        "Test User",
        "G6 author preserved"
    );

    // G5: leading squash refuses and writes nothing.
    let (repo, a, _b, _c) = setup_abc();
    let root = repo.path();
    let head = rev_parse(root, "HEAD");
    let index = ls_files(root);
    let editor = write_todo_editor(root, "g5", &format!("squash {} # A\n", abbrev7(&a)));
    let g5 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert_eq!(g5.status.code(), Some(128), "G5");
    let (stderr, report) = parse_cli_error_stderr(&g5.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
    assert!(
        stderr.contains("cannot 'squash' without a previous commit"),
        "G5: {stderr}"
    );
    assert_zero_writes(root, &head, &index);

    // G2 / I5: reword replaces the message.
    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let seq = write_todo_editor(
        root,
        "g2-todo",
        &format!("pick {} # A\nreword {} # B\n", abbrev7(&a), abbrev7(&b)),
    );
    let msg = write_script(
        root,
        ".g2-editor.sh",
        "#!/bin/sh\nprintf 'rewritten\\n' > \"$1\"\n",
    );
    let g2 = rebase_i(
        root,
        &[
            ("GIT_SEQUENCE_EDITOR", seq.as_str()),
            ("GIT_EDITOR", msg.as_str()),
        ],
    );
    assert!(
        g2.status.success(),
        "G2: {}",
        String::from_utf8_lossy(&g2.stderr)
    );
    let body = log_body(root, "HEAD");
    assert!(body.contains("rewritten"), "G2: {body}");
    assert_eq!(log_subjects(root)[0], "rewritten");

    // G3: fixup -C keeps this commit's message; -c then edits.
    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let seq = write_todo_editor(
        root,
        "g3c",
        &format!("pick {} # A\nfixup -C {} # B\n", abbrev7(&a), abbrev7(&b)),
    );
    let g3c = rebase_i(
        root,
        &[
            ("GIT_SEQUENCE_EDITOR", seq.as_str()),
            ("GIT_EDITOR", "true"),
        ],
    );
    assert!(
        g3c.status.success(),
        "G3 -C: {}",
        String::from_utf8_lossy(&g3c.stderr)
    );
    assert_eq!(log_subjects(root)[0], "B");

    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let seq = write_todo_editor(
        root,
        "g3c-edit",
        &format!("pick {} # A\nfixup -c {} # B\n", abbrev7(&a), abbrev7(&b)),
    );
    let msg = write_script(
        root,
        ".g3-editor.sh",
        "#!/bin/sh\nprintf 'from-c\\n' > \"$1\"\n",
    );
    let g3e = rebase_i(
        root,
        &[
            ("GIT_SEQUENCE_EDITOR", seq.as_str()),
            ("GIT_EDITOR", msg.as_str()),
        ],
    );
    assert!(
        g3e.status.success(),
        "G3 -c: {}",
        String::from_utf8_lossy(&g3e.stderr)
    );
    assert_eq!(log_subjects(root)[0], "from-c");
}

#[test]
fn test_rebase_i_stop_commands_matrix() {
    let setup_abc = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "a.txt", "A", "A");
        commit_file(&root, "b.txt", "B", "B");
        commit_file(&root, "c.txt", "C", "C");
        let a = rev_parse(&root, "HEAD~2");
        let b = rev_parse(&root, "HEAD~1");
        let c = rev_parse(&root, "HEAD");
        (repo, a, b, c)
    };

    // S1 / I6: edit stops, amend, then --continue finishes.
    let (repo, a, b, c) = setup_abc();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "s1",
        &format!(
            "pick {} # A\nedit {} # B\npick {} # C\n",
            abbrev7(&a),
            abbrev7(&b),
            abbrev7(&c)
        ),
    );
    let s1 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        s1.status.success(),
        "S1 stop: {}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let s1_err = String::from_utf8_lossy(&s1.stderr);
    assert!(
        s1_err.contains("Stopped at") && s1_err.contains("..."),
        "S1 stopped-at: {s1_err}"
    );
    assert!(s1_err.contains('B'), "S1 subject: {s1_err}");
    assert!(
        s1_err.contains("commit --amend") && s1_err.contains("rebase --continue"),
        "S1 amend hint: {s1_err}"
    );
    let status = run_libra_command(&["status"], root);
    assert_cli_success(&status, "S9 status during edit");
    let status_text = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status_text.contains("rebase in progress"),
        "S9: {status_text}"
    );
    fs::write(root.join("b.txt"), "B-amended\n").expect("amend file");
    assert_cli_success(&run_libra_command(&["add", "b.txt"], root), "stage amend");
    assert_cli_success(
        &run_libra_command(
            &["commit", "--amend", "-m", "amended-B", "--no-verify"],
            root,
        ),
        "S1 amend",
    );
    let cont = run_libra_command(&["rebase", "--continue"], root);
    assert!(
        cont.status.success(),
        "S1 continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(log_subjects(root)[0], "C");
    assert_eq!(log_subjects(root)[1], "amended-B");
    assert_eq!(current_branch(root).as_str(), "main");
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "S1 clears aux"
    );

    // S2 / I7: break stops, then --continue finishes.
    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "s2",
        &format!(
            "pick {} # A\nbreak\npick {} # B\n",
            abbrev7(&a),
            abbrev7(&b)
        ),
    );
    let s2 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        s2.status.success(),
        "S2 stop: {}",
        String::from_utf8_lossy(&s2.stderr)
    );
    let s2_err = String::from_utf8_lossy(&s2.stderr);
    assert!(
        s2_err.contains("Stopped at") && s2_err.contains("(A)"),
        "S2: {s2_err}"
    );
    let cont = run_libra_command(&["rebase", "--continue"], root);
    assert!(
        cont.status.success(),
        "S2 continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(log_subjects(root)[0], "B");
    assert_eq!(current_branch(root).as_str(), "main");

    // S3: exec prints Executing and command output.
    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "s3",
        &format!(
            "pick {} # A\nexec /bin/echo hello-exec\npick {} # B\n",
            abbrev7(&a),
            abbrev7(&b)
        ),
    );
    let s3 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        s3.status.success(),
        "S3: {}",
        String::from_utf8_lossy(&s3.stderr)
    );
    let s3_out = format!(
        "{}{}",
        String::from_utf8_lossy(&s3.stdout),
        String::from_utf8_lossy(&s3.stderr)
    );
    assert!(
        s3_out.contains("Executing: /bin/echo hello-exec"),
        "S3 executing: {s3_out}"
    );
    assert!(s3_out.contains("hello-exec"), "S3 stdout: {s3_out}");
    assert_eq!(log_subjects(root)[0], "B");

    // S4 / I12: exec false warns, stops; --continue finishes remaining.
    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "s4",
        &format!(
            "pick {} # A\nexec false\npick {} # B\n",
            abbrev7(&a),
            abbrev7(&b)
        ),
    );
    let s4 = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(!s4.status.success(), "S4 must stop");
    let s4_err = String::from_utf8_lossy(&s4.stderr);
    assert!(
        s4_err.contains("execution failed: false"),
        "S4 warning: {s4_err}"
    );
    let cont = run_libra_command(&["rebase", "--continue"], root);
    assert!(
        cont.status.success(),
        "S4 continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(log_subjects(root)[0], "B");

    // S5 / I7: --edit-todo shows remaining + ongoing hint; edits take effect.
    let (repo, a, b, _c) = setup_abc();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "s5-start",
        &format!(
            "pick {} # A\nbreak\npick {} # B\n",
            abbrev7(&a),
            abbrev7(&b)
        ),
    );
    let started = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", editor.as_str())]);
    assert!(
        started.status.success(),
        "S5 start: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let captured = root.join(".s5-captured.todo");
    let capture = write_capture_editor(root, &captured);
    let edit = run_libra_command_with_env(
        &["rebase", "--edit-todo"],
        root,
        &[("GIT_SEQUENCE_EDITOR", capture.as_str())],
    );
    assert!(
        edit.status.success(),
        "S5 capture: {}",
        String::from_utf8_lossy(&edit.stderr)
    );
    let captured_text = fs::read_to_string(&captured).expect("captured todo");
    assert!(
        captured_text.contains(&format!("pick {}", abbrev7(&b)))
            || captured_text.contains(&b[..7.min(b.len())]),
        "S5 remaining B: {captured_text}"
    );
    assert!(
        !captured_text.contains(&format!("pick {}", abbrev7(&a))),
        "S5 must omit applied A: {captured_text}"
    );
    assert!(
        captured_text.contains("You are editing the todo file of an ongoing interactive rebase"),
        "S5 hint: {captured_text}"
    );

    let rewrite = write_todo_editor(
        root,
        "s5-rewrite",
        &format!("exec /bin/echo from-edit-todo\npick {} # B\n", abbrev7(&b)),
    );
    let rewritten = run_libra_command_with_env(
        &["rebase", "--edit-todo"],
        root,
        &[("GIT_SEQUENCE_EDITOR", rewrite.as_str())],
    );
    assert!(
        rewritten.status.success(),
        "S5 rewrite: {}",
        String::from_utf8_lossy(&rewritten.stderr)
    );
    let cont = run_libra_command(&["rebase", "--continue"], root);
    assert!(
        cont.status.success(),
        "S5 continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    let cont_out = format!(
        "{}{}",
        String::from_utf8_lossy(&cont.stdout),
        String::from_utf8_lossy(&cont.stderr)
    );
    assert!(
        cont_out.contains("from-edit-todo"),
        "S5 edited exec: {cont_out}"
    );
    assert_eq!(log_subjects(root)[0], "B");

    // S6: --edit-todo without rebase, and during non-interactive rebase.
    let (repo, _a, _b, _c) = setup_abc();
    let root = repo.path();
    let head = rev_parse(root, "HEAD");
    let index = ls_files(root);
    let none = run_libra_command(&["rebase", "--edit-todo"], root);
    assert_eq!(none.status.code(), Some(128), "S6 no rebase");
    let (_, report) = parse_cli_error_stderr(&none.stderr);
    assert_eq!(report.error_code, "LBR-REPO-003");
    assert_zero_writes(root, &head, &index);

    let conflict_setup = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "conflict.txt", "base", "Base");
        assert_cli_success(
            &run_libra_command(&["switch", "-c", "feature"], &root),
            "create feature",
        );
        commit_file(
            &root,
            "conflict.txt",
            "feature",
            "Feature modifies conflict.txt",
        );
        assert_cli_success(
            &run_libra_command(&["switch", "main"], &root),
            "switch main",
        );
        commit_file(&root, "conflict.txt", "main", "Main modifies conflict.txt");
        assert_cli_success(
            &run_libra_command(&["switch", "feature"], &root),
            "switch feature",
        );
        let orig = rev_parse(&root, "HEAD");
        let index = ls_files(&root);
        (repo, orig, index)
    };

    let (repo, orig, _index) = conflict_setup();
    let root = repo.path();
    let non_int = run_libra_command(&["rebase", "main"], root);
    assert_eq!(non_int.status.code(), Some(128), "S6 non-interactive stop");
    let head_after_stop = rev_parse(root, "HEAD");
    let before = fs::read(root.join(".libra").join("rebase-aux.json")).ok();
    let denied = run_libra_command(&["rebase", "--edit-todo"], root);
    assert!(!denied.status.success(), "S6 non-interactive --edit-todo");
    let denied_err = String::from_utf8_lossy(&denied.stderr);
    assert!(
        denied_err.contains("interactive rebase") || denied_err.contains("--edit-todo"),
        "S6: {denied_err}"
    );
    assert_eq!(
        rev_parse(root, "HEAD"),
        head_after_stop,
        "S6 HEAD unchanged"
    );
    let after = fs::read(root.join(".libra").join("rebase-aux.json")).ok();
    assert_eq!(before, after, "S6 aux unchanged");
    assert_cli_success(&run_libra_command(&["rebase", "--abort"], root), "S6 abort");
    assert_eq!(rev_parse(root, "HEAD"), orig, "S6 abort restores");

    // S7: invalid line, --edit-todo, then --continue.
    let (repo, a, b, c) = setup_abc();
    let root = repo.path();
    let onto = rev_parse(root, "HEAD~3");
    let bad = write_todo_editor(root, "s7-bad", "frobnicate abcdef0\n");
    let halted = rebase_i(root, &[("GIT_SEQUENCE_EDITOR", bad.as_str())]);
    assert_eq!(halted.status.code(), Some(128), "S7 halt");
    assert_eq!(rev_parse(root, "HEAD"), onto, "S7 at onto");
    let fix = write_todo_editor(
        root,
        "s7-fix",
        &format!(
            "pick {} # A\npick {} # B\npick {} # C\n",
            abbrev7(&a),
            abbrev7(&b),
            abbrev7(&c)
        ),
    );
    let fixed = run_libra_command_with_env(
        &["rebase", "--edit-todo"],
        root,
        &[("GIT_SEQUENCE_EDITOR", fix.as_str())],
    );
    assert!(
        fixed.status.success(),
        "S7 edit-todo: {}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    let cont = run_libra_command(&["rebase", "--continue"], root);
    assert!(
        cont.status.success(),
        "S7 continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(log_subjects(root)[0], "C");
    assert_eq!(current_branch(root).as_str(), "main");

    // S8 / t3404:283: conflict --skip finishes remaining.
    let (repo, _orig, _index) = conflict_setup();
    let root = repo.path();
    let s8 = rebase_i_onto(root, "main", &[("GIT_SEQUENCE_EDITOR", "true")]);
    assert_eq!(s8.status.code(), Some(128), "S8 conflict");
    let skip = run_libra_command(&["rebase", "--skip"], root);
    assert!(
        skip.status.success(),
        "S8 skip: {}",
        String::from_utf8_lossy(&skip.stderr)
    );
    assert_eq!(current_branch(root).as_str(), "feature");
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "S8 clears aux"
    );
}

/// M-RI Q1, Q2, Q4–Q8 (HF-24): `-i` combinations and the public entry.
#[test]
fn test_rebase_i_combinations_matrix() {
    let setup_fixup = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "a.txt", "A", "A");
        commit_file(&root, "b.txt", "B", "B");
        commit_file(&root, "a.txt", "A\nfixup", "fixup! A");
        let a = rev_parse(&root, "HEAD~2");
        let b = rev_parse(&root, "HEAD~1");
        let fixup = rev_parse(&root, "HEAD");
        (repo, a, b, fixup)
    };

    // Q1 / I10: `-i --autosquash` rearranges `fixup! A` next to A.
    let (repo, a, b, fixup) = setup_fixup();
    let root = repo.path();
    let captured = root.join(".q1.todo");
    let editor = write_capture_editor(root, &captured);
    let q1 = run_libra_command_with_env(
        &["rebase", "-i", "--autosquash", "HEAD~3"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        q1.status.success(),
        "Q1: {}",
        String::from_utf8_lossy(&q1.stderr)
    );
    let todo = fs::read_to_string(&captured).expect("Q1 todo");
    let pick_a = format!("pick {}", abbrev7(&a));
    let fixup_line = format!("fixup {}", abbrev7(&fixup));
    let pick_b = format!("pick {}", abbrev7(&b));
    let a_pos = todo.find(&pick_a).expect("Q1 pick A");
    let fixup_pos = todo.find(&fixup_line).expect("Q1 fixup after A");
    let b_pos = todo.find(&pick_b).expect("Q1 pick B");
    assert!(
        a_pos < fixup_pos && fixup_pos < b_pos,
        "Q1 order A, fixup, B: {todo}"
    );
    assert_eq!(log_subjects(root)[0], "B");
    assert!(
        !log_subjects(root).iter().any(|s| s.contains("fixup!")),
        "Q1 folded: {:?}",
        log_subjects(root)
    );

    // Q2: `rebase.autosquash=true` + `-i` matches Q1; without `-i` it still
    // does not fold a linear history (M-AUTOSQUASH A6).
    let (repo, a, _b, fixup) = setup_fixup();
    let root = repo.path();
    assert_cli_success(
        &run_libra_command(&["config", "rebase.autosquash", "true"], root),
        "Q2 config",
    );
    let captured = root.join(".q2.todo");
    let editor = write_capture_editor(root, &captured);
    let q2 = run_libra_command_with_env(
        &["rebase", "-i", "HEAD~3"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        q2.status.success(),
        "Q2: {}",
        String::from_utf8_lossy(&q2.stderr)
    );
    let todo = fs::read_to_string(&captured).expect("Q2 todo");
    assert!(
        todo.contains(&format!("pick {}", abbrev7(&a)))
            && todo.contains(&format!("fixup {}", abbrev7(&fixup))),
        "Q2 config autosquash: {todo}"
    );
    assert!(
        !log_subjects(root).iter().any(|s| s.contains("fixup!")),
        "Q2 folded: {:?}",
        log_subjects(root)
    );

    let (repo, _, _, _) = setup_fixup();
    let root = repo.path();
    assert_cli_success(
        &run_libra_command(&["config", "rebase.autosquash", "true"], root),
        "Q2 A6 config",
    );
    let a6 = run_libra_command(&["rebase", "HEAD~3"], root);
    assert!(
        a6.status.success(),
        "Q2 A6: {}",
        String::from_utf8_lossy(&a6.stderr)
    );
    assert!(
        log_subjects(root).iter().any(|s| s.contains("fixup!")),
        "Q2 without -i must not fold: {:?}",
        log_subjects(root)
    );

    // Q4 / I11 / t3404:1041,:1080: `-i --root` starts at the root; reorder,
    // drop, and fixup of the root succeed.
    let setup_root = || {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "a.txt", "A", "A");
        commit_file(&root, "b.txt", "B", "B");
        let init = rev_parse(&root, "HEAD~2");
        let a = rev_parse(&root, "HEAD~1");
        let b = rev_parse(&root, "HEAD");
        (repo, init, a, b)
    };

    let (repo, init, a, b) = setup_root();
    let root = repo.path();
    let captured = root.join(".q4.todo");
    let editor = write_capture_editor(root, &captured);
    let q4 = run_libra_command_with_env(
        &["rebase", "-i", "--root"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        q4.status.success(),
        "Q4 capture: {}",
        String::from_utf8_lossy(&q4.stderr)
    );
    let todo = fs::read_to_string(&captured).expect("Q4 todo");
    assert!(
        todo.contains(&format!("pick {} # init", abbrev7(&init)))
            || todo.contains(&format!("pick {}", abbrev7(&init))),
        "Q4 starts at root: {todo}"
    );
    assert!(
        todo.contains(&abbrev7(&a)) && todo.contains(&abbrev7(&b)),
        "{todo}"
    );

    let (repo, init, a, b) = setup_root();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "q4-reorder",
        &format!(
            "pick {} # init\npick {} # B\npick {} # A\n",
            abbrev7(&init),
            abbrev7(&b),
            abbrev7(&a)
        ),
    );
    let reorder = run_libra_command_with_env(
        &["rebase", "-i", "--root"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        reorder.status.success(),
        "Q4 reorder: {}",
        String::from_utf8_lossy(&reorder.stderr)
    );
    assert_eq!(log_subjects(root), vec!["A", "B", "init"]);

    let (repo, init, a, b) = setup_root();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "q4-drop",
        &format!(
            "pick {} # init\npick {} # A\ndrop {} # B\n",
            abbrev7(&init),
            abbrev7(&a),
            abbrev7(&b)
        ),
    );
    let dropped = run_libra_command_with_env(
        &["rebase", "-i", "--root"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        dropped.status.success(),
        "Q4 drop: {}",
        String::from_utf8_lossy(&dropped.stderr)
    );
    assert_eq!(log_subjects(root), vec!["A", "init"]);

    let (repo, init, a, b) = setup_root();
    let root = repo.path();
    let editor = write_todo_editor(
        root,
        "q4-fixup-root",
        &format!(
            "pick {} # init\nfixup {} # A\npick {} # B\n",
            abbrev7(&init),
            abbrev7(&a),
            abbrev7(&b)
        ),
    );
    let fixup_root = run_libra_command_with_env(
        &["rebase", "-i", "--root"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        fixup_root.status.success(),
        "Q4 fixup root: {}",
        String::from_utf8_lossy(&fixup_root.stderr)
    );
    assert_eq!(log_subjects(root), vec!["B", "init"]);
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).expect("a.txt"),
        "A\n"
    );

    // Q5 / t3404:114: `-i --exec` inserts `exec` after each generated pick.
    let (repo, a, b, c) = {
        let repo = create_repo();
        let root = repo.path().to_path_buf();
        commit_file(&root, "init.txt", "init", "init");
        commit_file(&root, "a.txt", "A", "A");
        commit_file(&root, "b.txt", "B", "B");
        commit_file(&root, "c.txt", "C", "C");
        let a = rev_parse(&root, "HEAD~2");
        let b = rev_parse(&root, "HEAD~1");
        let c = rev_parse(&root, "HEAD");
        (repo, a, b, c)
    };
    let root = repo.path();
    let captured = root.join(".q5.todo");
    let editor = write_capture_editor(root, &captured);
    let q5 = run_libra_command_with_env(
        &["rebase", "-i", "--exec", "/bin/true", "HEAD~3"],
        root,
        &[("GIT_SEQUENCE_EDITOR", editor.as_str())],
    );
    assert!(
        q5.status.success(),
        "Q5: {}",
        String::from_utf8_lossy(&q5.stderr)
    );
    let todo = fs::read_to_string(&captured).expect("Q5 todo");
    for oid in [&a, &b, &c] {
        let pick = format!("pick {}", abbrev7(oid));
        let pos = todo
            .find(&pick)
            .unwrap_or_else(|| panic!("Q5 missing {pick}: {todo}"));
        let after = &todo[pos + pick.len()..];
        assert!(
            after.contains("exec /bin/true"),
            "Q5 exec after {pick}: {todo}"
        );
    }
    assert_eq!(
        todo.matches("exec /bin/true").count(),
        3,
        "Q5 exec count: {todo}"
    );

    // Q6: `-i --autostash` restores tracked dirt after a successful replay.
    let repo = create_repo();
    let root = repo.path();
    commit_file(root, "init.txt", "init", "init");
    commit_file(root, "a.txt", "A", "A");
    commit_file(root, "b.txt", "B", "B");
    commit_file(root, "c.txt", "C", "C");
    fs::write(root.join("a.txt"), "dirty\n").expect("dirty");
    let q6 = run_libra_command_with_env(
        &["rebase", "-i", "--autostash", "HEAD~3"],
        root,
        &[("GIT_SEQUENCE_EDITOR", "true")],
    );
    assert!(
        q6.status.success(),
        "Q6: {}",
        String::from_utf8_lossy(&q6.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).expect("restored"),
        "dirty\n",
        "Q6 must restore autostashed dirt"
    );
    assert!(
        !root.join(".libra").join("rebase-aux.json").exists(),
        "Q6 clears aux"
    );

    // Q7: `-i --update-refs` is usage 129 + DEFER-02; `-r` stays declined.
    let repo = create_repo();
    let root = repo.path();
    commit_file(root, "init.txt", "init", "init");
    let head = rev_parse(root, "HEAD");
    let index = ls_files(root);
    let denied = run_libra_command(&["rebase", "-i", "--update-refs", "HEAD"], root);
    assert_eq!(denied.status.code(), Some(129), "Q7 update-refs");
    let (stderr, report) = parse_cli_error_stderr(&denied.stderr);
    assert_eq!(report.error_code, "LBR-CLI-002");
    assert!(
        stderr.contains("update-refs") && stderr.contains("DEFER-02"),
        "Q7 hint: {stderr}"
    );
    assert_zero_writes(root, &head, &index);

    let declined = run_libra_command(&["rebase", "--rebase-merges", "HEAD"], root);
    assert_eq!(declined.status.code(), Some(128), "Q7 -r");
    let (stderr, report) = parse_cli_error_stderr(&declined.stderr);
    assert_eq!(report.error_code, "LBR-UNSUPPORTED-001");
    assert!(
        stderr.contains("rebase-merges") || stderr.contains("not supported"),
        "Q7 declined: {stderr}"
    );
    assert_zero_writes(root, &head, &index);

    // Q8: public help lists `-i` / `--interactive` / `--edit-todo`.
    let help = run_libra_command(&["rebase", "--help"], root);
    assert!(help.status.success(), "Q8 help");
    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(
        help_text.contains("-i")
            && help_text.contains("--interactive")
            && help_text.contains("--edit-todo")
            && help_text.contains("libra rebase -i"),
        "Q8 help: {help_text}"
    );
}
