//! OL-02 focused coverage for the v1 -> v2 operation schema replacement.

use std::{collections::BTreeMap, path::Path};

use libra::internal::db;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use tempfile::TempDir;

async fn table_columns(conn: &sea_orm::DatabaseConnection, table: &str) -> Vec<String> {
    let statement =
        Statement::from_string(DbBackend::Sqlite, format!("PRAGMA table_info('{table}')"));
    let rows = conn
        .query_all_raw(statement)
        .await
        .expect("table_info query succeeds");
    rows.into_iter()
        .map(|row| row.try_get_by_index::<String>(1).expect("column name"))
        .collect()
}

async fn schema_signature(conn: &sea_orm::DatabaseConnection) -> BTreeMap<String, Vec<String>> {
    let rows = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN (\
             'operation', 'operation_parent', 'operation_head', 'operation_journal',\
             'change_identity', 'change_revision', 'change_predecessor', 'ai_operation_link',\
             'operation_view', 'operation_view_ref', 'operation_view_workspace')\
             ORDER BY name",
        ))
        .await
        .expect("schema table query succeeds");
    let mut signature = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get_by_index(0).expect("table name");
        signature.insert(name.clone(), table_columns(conn, &name).await);
    }
    signature
}

fn db_path(dir: &TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name)
}

async fn make_legacy_database(path: &Path) -> sea_orm::DatabaseConnection {
    let conn = db::create_database(path.to_str().expect("UTF-8 database path"))
        .await
        .expect("create baseline database");
    conn.execute_unprepared(
        "DROP TABLE operation_journal;\
         DROP TABLE operation_head;\
         DROP TABLE change_identity;\
         DROP TABLE change_revision;\
         DROP TABLE change_predecessor;\
         DROP TABLE ai_operation_link;\
         DROP TABLE operation_parent;\
         DROP TABLE operation;\
         CREATE TABLE operation (\
             op_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, view_id TEXT NOT NULL,\
             command_name TEXT NOT NULL, description TEXT NOT NULL, actor TEXT NOT NULL,\
             args_digest TEXT, start_ts INTEGER NOT NULL, end_ts INTEGER, status TEXT NOT NULL\
         );\
         CREATE TABLE operation_parent (\
             op_id TEXT NOT NULL, parent_op_id TEXT NOT NULL, PRIMARY KEY (op_id, parent_op_id)\
         );\
         CREATE TABLE operation_view (\
             view_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, head_kind TEXT NOT NULL,\
             head_target TEXT NOT NULL, created_at INTEGER NOT NULL\
         );\
         CREATE TABLE operation_view_ref (\
             view_id TEXT NOT NULL, ref_kind TEXT NOT NULL, ref_name TEXT NOT NULL,\
             ref_remote TEXT NOT NULL, target_oid TEXT NOT NULL,\
             PRIMARY KEY (view_id, ref_kind, ref_name, ref_remote)\
         );\
         CREATE TABLE operation_view_workspace (\
             view_id TEXT NOT NULL, pointer_kind TEXT NOT NULL, pointer_value TEXT NOT NULL,\
             PRIMARY KEY (view_id, pointer_kind)\
         );\
         DELETE FROM schema_versions WHERE version = 2026090101",
    )
    .await
    .expect("install legacy operation schema");
    conn
}

#[tokio::test]
async fn fresh_and_legacy_databases_converge_to_the_same_v2_schema() {
    let dir = TempDir::new().expect("temporary schema directory");
    let fresh_path = db_path(&dir, "fresh.db");
    let legacy_path = db_path(&dir, "legacy.db");

    let fresh = db::create_database(fresh_path.to_str().expect("UTF-8 path"))
        .await
        .expect("fresh database initializes");
    let fresh_signature = schema_signature(&fresh).await;
    assert!(!fresh_signature.contains_key("operation_view"));
    assert_eq!(
        fresh_signature
            .get("operation")
            .and_then(|columns| columns.first())
            .map(String::as_str),
        Some("op_id")
    );
    assert!(fresh_signature["operation"].contains(&"pre_view_oid".to_string()));
    assert!(fresh_signature.contains_key("operation_head"));
    assert!(fresh_signature.contains_key("operation_journal"));
    assert!(fresh_signature.contains_key("ai_operation_link"));
    drop(fresh);

    let legacy = make_legacy_database(&legacy_path).await;
    let legacy_signature = {
        db::upgrade_database_schema(&legacy_path)
            .await
            .expect("legacy database migrates forward");
        let upgraded = db::establish_connection(legacy_path.to_str().expect("UTF-8 path"))
            .await
            .expect("upgraded database opens");
        let signature = schema_signature(&upgraded).await;
        drop(upgraded);
        signature
    };
    drop(legacy);

    assert_eq!(fresh_signature, legacy_signature);
}

#[tokio::test]
async fn operation_v2_migration_is_versioned_and_exposes_v2_shape() {
    let dir = TempDir::new().expect("temporary schema directory");
    let path = db_path(&dir, "version.db");
    let conn = db::create_database(path.to_str().expect("UTF-8 path"))
        .await
        .expect("create database");
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT MAX(version) FROM schema_versions",
            [],
        ))
        .await
        .expect("schema version query")
        .expect("schema version row");
    let version: i64 = row.try_get_by_index(0).expect("schema version");
    assert_eq!(version, 2026090101);
    assert!(
        table_columns(&conn, "operation_parent")
            .await
            .contains(&"ordinal".to_string())
    );
}

#[tokio::test]
async fn empty_operation_v2_database_can_round_trip_through_guarded_rollback() {
    let dir = TempDir::new().expect("temporary schema directory");
    let path = db_path(&dir, "rollback.db");
    let conn = db::create_database(path.to_str().expect("UTF-8 path"))
        .await
        .expect("create database");
    let runner = libra::internal::db::migration::builtin_runner().expect("builtin runner");

    runner
        .rollback_to(&conn, 2026082401)
        .await
        .expect("empty v2 database rolls back");
    assert!(
        table_columns(&conn, "operation")
            .await
            .contains(&"view_id".to_string())
    );
    assert!(
        !table_columns(&conn, "operation")
            .await
            .contains(&"post_view_oid".to_string())
    );
    assert!(
        table_columns(&conn, "operation_view")
            .await
            .contains(&"head_target".to_string())
    );
    assert!(table_columns(&conn, "operation_head").await.is_empty());

    runner.run_pending(&conn).await.expect("v2 re-upgrade");
    assert!(
        table_columns(&conn, "operation")
            .await
            .contains(&"post_view_oid".to_string())
    );
    assert!(
        !table_columns(&conn, "operation")
            .await
            .contains(&"view_id".to_string())
    );
}
