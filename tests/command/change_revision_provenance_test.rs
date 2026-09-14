//! CLI regression coverage for operation ids attached to commit revisions.

use std::path::Path;

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseBackend, Statement};

use super::*;

async fn open_repo_db(repo: &Path) -> sea_orm::DatabaseConnection {
    let url = format!("sqlite://{}", repo.join(".libra/libra.db").display());
    let mut options = ConnectOptions::new(url);
    options.sqlx_logging(false);
    Database::connect(options)
        .await
        .expect("open repository operation database")
}

async fn assert_revision_resolves_to_store(
    repo: &Path,
    commit_oid: &str,
    store_table: &str,
    expected_command: Option<&str>,
) {
    let database = open_repo_db(repo).await;
    let revision = database
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT created_op_id FROM change_revision WHERE commit_oid = ?",
            [commit_oid.to_string().into()],
        ))
        .await
        .expect("query change revision")
        .expect("revision row for created commit");
    let op_id = revision
        .try_get_by_index::<String>(0)
        .expect("revision operation id");

    let sql = match expected_command {
        Some(_) if store_table == "legacy_operation" => {
            "SELECT command_name FROM legacy_operation WHERE op_id = ?"
        }
        _ => {
            let count_sql = format!("SELECT COUNT(*) FROM {store_table} WHERE op_id = ?");
            let row = database
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    count_sql,
                    [op_id.clone().into()],
                ))
                .await
                .expect("resolve revision operation id")
                .expect("operation count row");
            let count = row.try_get_by_index::<i64>(0).expect("operation count");
            assert_eq!(
                count, 1,
                "revision {commit_oid} must resolve to exactly one {store_table} operation"
            );
            return;
        }
    };

    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            [op_id.into()],
        ))
        .await
        .expect("resolve revision operation id")
        .expect("operation row for revision provenance");
    let command_name = row
        .try_get_by_index::<String>(0)
        .expect("operation command name");
    assert_eq!(Some(command_name.as_str()), expected_command);
}

fn head_oid(repo: &Path) -> String {
    let output = run_libra_command(&["rev-parse", "HEAD"], repo);
    assert_cli_success(&output, "read HEAD");
    String::from_utf8(output.stdout)
        .expect("HEAD output is UTF-8")
        .trim()
        .to_string()
}

#[tokio::test]
async fn commit_and_amend_revisions_resolve_to_the_v2_operation_store() {
    let repo = tempdir().expect("temporary repository");
    init_repo_via_cli(repo.path());
    configure_identity_via_cli(repo.path());

    fs::write(repo.path().join("tracked.txt"), "initial\n").expect("write tracked file");
    assert_cli_success(
        &run_libra_command(&["add", "tracked.txt"], repo.path()),
        "stage initial file",
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "initial", "--no-verify"], repo.path()),
        "initial commit",
    );
    let initial_oid = head_oid(repo.path());
    assert_revision_resolves_to_store(repo.path(), &initial_oid, "operation", None).await;

    fs::write(repo.path().join("tracked.txt"), "amended\n").expect("update tracked file");
    assert_cli_success(
        &run_libra_command(&["add", "tracked.txt"], repo.path()),
        "stage amended file",
    );
    assert_cli_success(
        &run_libra_command(
            &["commit", "--amend", "--no-edit", "--no-verify"],
            repo.path(),
        ),
        "amend commit",
    );
    let amended_oid = head_oid(repo.path());
    assert_ne!(amended_oid, initial_oid);
    assert_revision_resolves_to_store(repo.path(), &amended_oid, "operation", None).await;
}

#[tokio::test]
async fn cherry_pick_and_rebase_revisions_resolve_to_their_control_operations() {
    let repo = tempdir().expect("temporary repository");
    init_repo_via_cli(repo.path());
    configure_identity_via_cli(repo.path());

    fs::write(repo.path().join("base.txt"), "base\n").expect("write base");
    assert_cli_success(
        &run_libra_command(&["add", "base.txt"], repo.path()),
        "stage base",
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "base", "--no-verify"], repo.path()),
        "base commit",
    );

    assert_cli_success(
        &run_libra_command(&["switch", "-c", "feature"], repo.path()),
        "create feature branch",
    );
    fs::write(repo.path().join("feature.txt"), "feature\n").expect("write feature");
    assert_cli_success(
        &run_libra_command(&["add", "feature.txt"], repo.path()),
        "stage feature",
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "feature", "--no-verify"], repo.path()),
        "feature commit",
    );

    assert_cli_success(
        &run_libra_command(&["switch", "main"], repo.path()),
        "switch to main",
    );
    fs::write(repo.path().join("main.txt"), "main\n").expect("write main");
    assert_cli_success(
        &run_libra_command(&["add", "main.txt"], repo.path()),
        "stage main",
    );
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "main", "--no-verify"], repo.path()),
        "main commit",
    );

    assert_cli_success(
        &run_libra_command(&["switch", "feature"], repo.path()),
        "switch to feature",
    );
    assert_cli_success(
        &run_libra_command(&["rebase", "main"], repo.path()),
        "rebase feature onto main",
    );
    let rebased_oid = head_oid(repo.path());
    assert_revision_resolves_to_store(
        repo.path(),
        &rebased_oid,
        "legacy_operation",
        Some("rebase"),
    )
    .await;

    assert_cli_success(
        &run_libra_command(&["switch", "main"], repo.path()),
        "switch to main for cherry-pick",
    );
    assert_cli_success(
        &run_libra_command(&["cherry-pick", "feature"], repo.path()),
        "cherry-pick feature",
    );
    let picked_oid = head_oid(repo.path());
    assert_revision_resolves_to_store(
        repo.path(),
        &picked_oid,
        "legacy_operation",
        Some("cherry-pick"),
    )
    .await;
}
