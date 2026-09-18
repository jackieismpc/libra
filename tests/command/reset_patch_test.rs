//! `reset -p` session (HF-20 / M-RESETP / t7105).

use std::{fs, path::Path, process::Output};

use tempfile::tempdir;

use super::{
    assert_cli_success, configure_identity_via_cli, init_repo_via_cli, parse_cli_error_stderr,
    run_libra_command, run_libra_command_with_stdin,
};

fn create_repo() -> tempfile::TempDir {
    let repo = tempdir().expect("tempdir");
    init_repo_via_cli(repo.path());
    configure_identity_via_cli(repo.path());
    repo
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write_line(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, format!("{body}\n")).unwrap();
}

fn read_line(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn commit_path(repo: &Path, rel: &str, body: &str, message: &str) {
    write_line(&repo.join(rel), body);
    assert_cli_success(
        &run_libra_command(&["add", "--", rel], repo),
        &format!("add {rel}"),
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", message, "--no-verify"], repo),
        message,
    );
}

fn set_state(repo: &Path, rel: &str, work: &str, index: &str) {
    write_line(&repo.join(rel), index);
    assert_cli_success(
        &run_libra_command(&["add", "--", rel], repo),
        &format!("index {rel}"),
    );
    write_line(&repo.join(rel), work);
}

fn ls_files_line(repo: &Path, rel: &str) -> String {
    let output = run_libra_command(&["ls-files", "-s", "--", rel], repo);
    assert_cli_success(&output, &format!("ls-files {rel}"));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn rev_parse(repo: &Path, rev: &str) -> String {
    let output = run_libra_command(&["rev-parse", rev], repo);
    assert_cli_success(&output, &format!("rev-parse {rev}"));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct SavedPath {
    work: String,
    index: String,
}

fn save_state(repo: &Path, rel: &str) -> SavedPath {
    SavedPath {
        work: read_line(&repo.join(rel)),
        index: ls_files_line(repo, rel),
    }
}

fn verify_saved(repo: &Path, rel: &str, saved: &SavedPath) {
    assert_eq!(read_line(&repo.join(rel)), saved.work, "worktree {rel}");
    assert_eq!(ls_files_line(repo, rel), saved.index, "index {rel}");
}

fn verify_state(repo: &Path, rel: &str, work: &str, index_via: IndexExpect) {
    assert_eq!(
        read_line(&repo.join(rel)),
        format!("{work}\n"),
        "worktree {rel}"
    );
    match index_via {
        IndexExpect::Hash(hash) => {
            assert!(
                ls_files_line(repo, rel).contains(&hash),
                "index {rel} missing {hash}: {}",
                ls_files_line(repo, rel)
            );
        }
    }
}

enum IndexExpect {
    Hash(String),
}

fn blob_hash_at(repo: &Path, spec: &str) -> String {
    let output = run_libra_command(&["rev-parse", spec], repo);
    assert_cli_success(&output, spec);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn reset_patch(repo: &Path, extra: &[&str], stdin_body: &str) -> Output {
    let mut args = vec!["reset", "-p"];
    args.extend(extra);
    run_libra_command_with_stdin(&args, repo, stdin_body)
}

fn setup_t7105(repo: &Path) -> (SavedPath, String) {
    fs::create_dir_all(repo.join("dir")).unwrap();
    commit_path(repo, "dir/foo", "parent", "initial");
    commit_path(repo, "dir/foo", "head", "second");
    set_state(repo, "bar", "bar_work", "bar_index");
    let bar = save_state(repo, "bar");
    let head = rev_parse(repo, "HEAD");
    (bar, head)
}

#[test]
fn test_t7105_reset_patch_matrix() {
    let repo = create_repo();
    let root = repo.path();
    let (bar, head) = setup_t7105(root);
    let foo_head = blob_hash_at(root, "HEAD:dir/foo");
    let foo_parent = blob_hash_at(root, "HEAD^:dir/foo");

    // R1: n n leaves index and worktree unchanged.
    set_state(root, "dir/foo", "work", "work");
    let foo = save_state(root, "dir/foo");
    let r1 = reset_patch(root, &[], "n\nn\n");
    assert_cli_success(&r1, "R1");
    verify_saved(root, "dir/foo", &foo);
    verify_saved(root, "bar", &bar);

    // R2: reset -p / HEAD / @ with n y unstages only dir/foo.
    for extra in [&[] as &[&str], &["HEAD"], &["@"]] {
        set_state(root, "dir/foo", "work", "work");
        let output = reset_patch(root, extra, "n\ny\n");
        assert_cli_success(&output, &format!("R2 {extra:?}"));
        assert!(
            stdout(&output).contains("Unstage"),
            "R2 {extra:?}: {}",
            stdout(&output)
        );
        verify_state(root, "dir/foo", "work", IndexExpect::Hash(foo_head.clone()));
        verify_saved(root, "bar", &bar);
        set_state(root, "dir/foo", "work", "work");
    }

    // R3: HEAD^ and HEAD^^{{tree}} apply reverse hunks onto the index.
    for extra in [&["HEAD^"] as &[&str], &["HEAD^^{tree}"]] {
        set_state(root, "dir/foo", "work", "work");
        let output = reset_patch(root, extra, "n\ny\n");
        assert_cli_success(&output, &format!("R3 {extra:?}"));
        let text = stdout(&output);
        assert!(text.contains("Apply"), "R3 {extra:?}: {text}");
        assert!(
            text.contains("diff --git b/dir/foo a/dir/foo")
                || text.contains("diff --git b/bar a/bar"),
            "R3 reverse header {extra:?}: {text}"
        );
        verify_state(
            root,
            "dir/foo",
            "work",
            IndexExpect::Hash(foo_parent.clone()),
        );
        verify_saved(root, "bar", &bar);
        set_state(root, "dir/foo", "work", "work");
    }

    // R4: blob / unknown targets fail closed with 129 + LBR-CLI-003.
    set_state(root, "dir/foo", "work", "work");
    let foo = save_state(root, "dir/foo");
    for target in ["HEAD^:dir/foo", "aaaaaaaa"] {
        let output = reset_patch(root, &[target], "y\n");
        assert_eq!(output.status.code(), Some(129), "{target}");
        let (_, report) = parse_cli_error_stderr(&output.stderr);
        assert_eq!(report.error_code, "LBR-CLI-003", "{target}");
        verify_saved(root, "dir/foo", &foo);
        verify_saved(root, "bar", &bar);
    }

    // R5: pathspecs only touch matching paths (bar sorts first; y applies to
    // the limited path, extra n would catch a limiter failure).
    set_state(root, "dir/foo", "work", "work");
    let r5_dir = reset_patch(root, &["dir"], "y\nn\n");
    assert_cli_success(&r5_dir, "R5 dir");
    verify_state(root, "dir/foo", "work", IndexExpect::Hash(foo_head.clone()));
    verify_saved(root, "bar", &bar);

    set_state(root, "dir/foo", "work", "work");
    let r5_foo =
        run_libra_command_with_stdin(&["reset", "-p", "--", "foo"], &root.join("dir"), "y\nn\n");
    assert_cli_success(&r5_foo, "R5 -- foo");
    verify_state(root, "dir/foo", "work", IndexExpect::Hash(foo_head.clone()));
    verify_saved(root, "bar", &bar);

    set_state(root, "dir/foo", "work", "work");
    let r5_head = reset_patch(root, &["HEAD^", "--", "dir"], "y\nn\n");
    assert_cli_success(&r5_head, "R5 HEAD^ -- dir");
    verify_state(
        root,
        "dir/foo",
        "work",
        IndexExpect::Hash(foo_parent.clone()),
    );
    verify_saved(root, "bar", &bar);

    // R6: HEAD never moved.
    assert_eq!(rev_parse(root, "HEAD"), head, "R6 HEAD moved");

    // R7: --no-auto-advance with -p offers >/<; without -p is 128.
    set_state(root, "dir/foo", "work", "work");
    let r7 = run_libra_command_with_stdin(&["reset", "-p", "--no-auto-advance"], root, "?\nq\n");
    assert_cli_success(&r7, "R7 no-auto-advance");
    let text = stdout(&r7);
    assert!(text.contains('>') && text.contains('<'), "{text}");
    assert!(text.contains("Unstage"), "{text}");
    verify_saved(root, "bar", &bar);

    let r7_bare = run_libra_command(&["reset", "--no-auto-advance"], root);
    assert_eq!(r7_bare.status.code(), Some(128));
    let err = String::from_utf8_lossy(&r7_bare.stderr);
    assert!(
        err.contains("the option '--no-auto-advance' requires '--patch'"),
        "{err}"
    );

    // R8: mode flags and --json cannot combine with -p.
    let foo = save_state(root, "dir/foo");
    for flag in ["--soft", "--mixed", "--hard", "--merge", "--keep"] {
        let output = run_libra_command(&["reset", "-p", flag], root);
        assert_eq!(output.status.code(), Some(129), "{flag}");
        verify_saved(root, "dir/foo", &foo);
        verify_saved(root, "bar", &bar);
    }
    let json = run_libra_command(&["--json", "reset", "-p"], root);
    assert_eq!(json.status.code(), Some(129));
    let (_, report) = parse_cli_error_stderr(&json.stderr);
    assert_eq!(report.error_code, "LBR-CLI-002");
    verify_saved(root, "dir/foo", &foo);
    verify_saved(root, "bar", &bar);
}
