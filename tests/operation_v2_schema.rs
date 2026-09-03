//! OL-02 schema convergence tests.

use std::collections::BTreeMap;

use libra::internal::db::{create_database, migration::run_builtin_migrations};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use tempfile::tempdir;

const V2_TABLES: [&str; 8] = [
    "operation",
    "operation_parent",
    "operation_head",
    "operation_journal",
    "change_identity",
    "change_revision",
    "change_predecessor",
    "ai_operation_link",
];

async fn schema_columns(conn: &DatabaseConnection) -> BTreeMap<String, Vec<String>> {
    let mut schema = BTreeMap::new();
    for table in V2_TABLES {
        let rows = conn
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("PRAGMA table_info('{table}')"),
            ))
            .await
            .expect("read table info");
        let mut columns = rows
            .into_iter()
            .map(|row| row.try_get_by_index::<String>(1).expect("column name"))
            .collect::<Vec<_>>();
        columns.sort();
        schema.insert(table.to_string(), columns);
    }
    schema
}

async fn assert_only_v2_operation_tables(conn: &DatabaseConnection) {
    let rows = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND (name LIKE 'operation%' OR name LIKE 'change_%' OR name = 'ai_operation_link') ORDER BY name".to_string(),
        ))
        .await
        .expect("list operation tables");
    let names = rows
        .into_iter()
        .map(|row| row.try_get_by_index::<String>(0).expect("table name"))
        .collect::<Vec<_>>();
    let mut expected = V2_TABLES
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(names, expected);
}

#[tokio::test]
async fn fresh_database_uses_operation_v2_schema() {
    let dir = tempdir().expect("temp directory");
    let db_path = dir.path().join("fresh.db");
    let conn = create_database(db_path.to_str().expect("db path")).await.expect("create db");

    assert_only_v2_operation_tables(&conn).await;
    let schema = schema_columns(&conn).await;
    assert_eq!(schema["operation"].len(), 19);
    assert_eq!(schema["operation_head"].len(), 4);
    assert_eq!(schema["operation_journal"].len(), 8);
    assert_eq!(schema["change_identity"].len(), 5);
    assert_eq!(schema["change_revision"].len(), 5);
    assert_eq!(schema["change_predecessor"].len(), 5);
    assert_eq!(schema["ai_operation_link"].len(), 11);
}

#[tokio::test]
async fn legacy_operation_schema_converges_to_v2() {
    let dir = tempdir().expect("temp directory");
    let db_path = dir.path().join("legacy.db");
    let conn = create_database(db_path.to_str().expect("db path")).await.expect("create db");

    conn.execute_unprepared(
        "DROP TABLE IF EXISTS operation_view_workspace; DROP TABLE IF EXISTS operation_view_ref; DROP TABLE IF EXISTS operation_view; DROP TABLE IF EXISTS operation_journal; DROP TABLE IF EXISTS operation_head; DROP TABLE IF EXISTS operation_parent; DROP TABLE IF EXISTS operation; DROP TABLE IF EXISTS change_identity; DROP TABLE IF EXISTS change_revision; DROP TABLE IF EXISTS change_predecessor; DROP TABLE IF EXISTS ai_operation_link; CREATE TABLE operation(op_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, view_id TEXT NOT NULL, command_name TEXT NOT NULL, description TEXT NOT NULL, actor TEXT NOT NULL, args_digest TEXT, start_ts INTEGER NOT NULL, end_ts INTEGER, status TEXT NOT NULL); CREATE TABLE operation_parent(op_id TEXT NOT NULL, parent_op_id TEXT NOT NULL, PRIMARY KEY(op_id, parent_op_id)); CREATE TABLE operation_view(view_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, head_kind TEXT NOT NULL, head_target TEXT NOT NULL, created_at INTEGER NOT NULL); CREATE TABLE operation_view_ref(view_id TEXT NOT NULL, ref_kind TEXT NOT NULL, ref_name TEXT NOT NULL, ref_remote TEXT NOT NULL, target_oid TEXT NOT NULL, PRIMARY KEY(view_id, ref_kind, ref_name, ref_remote)); CREATE TABLE operation_view_workspace(view_id TEXT NOT NULL, pointer_kind TEXT NOT NULL, pointer_value TEXT NOT NULL, PRIMARY KEY(view_id, pointer_kind)); DELETE FROM schema_versions WHERE version = 2026090301",
    )
    .await
    .expect("plant legacy schema");

    run_builtin_migrations(&conn).await.expect("upgrade legacy schema");
    assert_only_v2_operation_tables(&conn).await;
}
