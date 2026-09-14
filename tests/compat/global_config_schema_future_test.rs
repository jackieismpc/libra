//! Configuration-role compatibility policy and read-only dispatch guards.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use sea_orm::{ConnectionTrait, Statement};
use tempfile::{TempDir, tempdir};

const SECRET_VALUE: &str = "SECRET_SCHEMA_FUTURE_SHOULD_NOT_LEAK";
const ENV_SECRET_VALUE: &str = "ENV_STORAGE_SECRET_SHOULD_NOT_LEAK";
const INSTALL_COMMAND: &str =
    "curl --proto '=https' --tlsv1.2 -sSf https://download.libra.tools/install.sh | sh";

fn fixture_sql(path: &Path, sql: &str) {
    tokio::runtime::Runtime::new()
        .expect("fixture runtime")
        .block_on(async {
            let conn = raw_config_fixture(path).await;
            conn.execute_unprepared(sql).await.expect("fixture SQL");
            conn.close().await.expect("close fixture writer");
        });
}

fn known_repository_receipts(path: &Path) {
    use libra::internal::db::{DatabaseRole, schema};
    tokio::runtime::Runtime::new()
        .expect("fixture runtime")
        .block_on(async {
            let conn = raw_config_fixture(path).await;
            conn.execute_unprepared(
                schema::schema_manifest()
                    .configuration_barrier
                    .legacy_ledger_sql,
            )
            .await
            .unwrap();
            for migration in schema::migrations_for_role(DatabaseRole::Repository) {
                conn.execute_raw(Statement::from_sql_and_values(
                    conn.get_database_backend(),
                    "INSERT INTO schema_versions VALUES (?, ?, 'fixture')",
                    [migration.version.into(), migration.name.into()],
                ))
                .await
                .unwrap();
            }
            libra::internal::config::ConfigKv::set_with_conn(
                &conn,
                "test.receipt",
                "preserved-value",
                false,
            )
            .await
            .unwrap();
            conn.close().await.unwrap();
        });
}

fn configuration_base_fixture(path: &Path, role: libra::internal::db::DatabaseRole) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let conn = libra::internal::db::create_database_for_role(path.to_str().unwrap(), role)
            .await
            .unwrap();
        conn.close().await.unwrap();
    });
}

fn assert_preflight_allowed(fixture: &CliFixture) {
    let missing = fixture.root.join("absent-source");
    for args in [
        vec!["pull"],
        vec!["push"],
        vec!["fetch"],
        vec!["cloud", "status"],
        vec!["clone", missing.to_str().unwrap(), "absent-destination"],
    ] {
        let output = fixture.run(&fixture.repo, &args);
        let stderr = stderr_text(&output);
        assert!(!stderr.contains("LBR-CONFIG-001"), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("unsupported migration receipt"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn repository_only_receipt_does_not_block_remote_commands() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    for (path, role) in [
        (
            &fixture.global_db,
            libra::internal::db::DatabaseRole::GlobalConfig,
        ),
        (
            &fixture.system_db,
            libra::internal::db::DatabaseRole::SystemConfig,
        ),
    ] {
        configuration_base_fixture(path, role);
        known_repository_receipts(path);
    }
    let snapshots = [
        fs::read(&fixture.global_db).unwrap(),
        fs::read(&fixture.system_db).unwrap(),
    ];
    assert_preflight_allowed(&fixture);
    assert_eq!(
        snapshots,
        [
            fs::read(&fixture.global_db).unwrap(),
            fs::read(&fixture.system_db).unwrap()
        ]
    );
}

#[test]
fn global_and_system_future_schema_fail_closed() {
    for system in [false, true] {
        let fixture = CliFixture::new();
        fixture.init_repo();
        fixture.write_future_global_config();
        if system {
            fs::create_dir_all(fixture.system_db.parent().unwrap()).unwrap();
            fs::copy(&fixture.global_db, &fixture.system_db).unwrap();
        }
        let path = if system {
            &fixture.system_db
        } else {
            &fixture.global_db
        };
        let before = fs::read(path).unwrap();
        for args in [
            vec!["pull"],
            vec!["push"],
            vec!["fetch"],
            vec!["cloud", "status"],
            vec!["clone", "https://example.invalid/never-contacted", "copy"],
        ] {
            let output = if system {
                // Global is also future, but its credentials are satisfied.
                // It must not hide the second, System issue.
                fixture.run_envs(
                    &fixture.repo,
                    &args,
                    &[
                        ("LIBRA_STORAGE_TYPE", "local"),
                        ("LIBRA_D1_ACCOUNT_ID", "fixture-account"),
                        ("LIBRA_D1_API_TOKEN", ENV_SECRET_VALUE),
                        ("LIBRA_D1_DATABASE_ID", "fixture-database"),
                    ],
                )
            } else {
                fixture.run(&fixture.repo, &args)
            };
            let stderr = stderr_text(&output);
            assert!(
                !output.status.success() && stderr.contains("LBR-CONFIG-001"),
                "{args:?}: {stderr}"
            );
            assert!(!stderr.contains(ENV_SECRET_VALUE));
            if system {
                assert!(
                    stderr.contains("system config database schema is newer"),
                    "{stderr}"
                );
            }
        }
        assert_eq!(before, fs::read(path).unwrap());
        // Even malformed unrelated metadata cannot hide a proven own-role
        // future behind the System unreadable-store carve-out.
        fixture_sql(path, "CREATE TABLE schema_versions (unexpected TEXT)");
        let output = fixture.run_env(
            &fixture.repo,
            &["pull"],
            "LIBRA_STORAGE_TYPE",
            if system { "local" } else { "r2" },
        );
        assert!(stderr_text(&output).contains("LBR-CONFIG-001"));
    }
}

#[test]
fn config_future_diagnostics_are_redacted() {
    for system in [false, true] {
        let fixture = CliFixture::new();
        fixture.init_repo();
        fixture.write_future_global_config();
        if system {
            fs::create_dir_all(fixture.system_db.parent().unwrap()).unwrap();
            fs::rename(&fixture.global_db, &fixture.system_db).unwrap();
        }
        let path = if system {
            &fixture.system_db
        } else {
            &fixture.global_db
        };
        fixture_sql(
            path,
            "UPDATE configuration_schema_versions SET name = 'SECRET_RECEIPT_NAME';",
        );
        for args in [vec!["pull"], vec!["--json", "pull"]] {
            let output = fixture.run(&fixture.repo, &args);
            let stderr = stderr_text(&output);
            assert!(stderr.contains("LBR-CONFIG-001"), "{stderr}");
            for secret in [SECRET_VALUE, ENV_SECRET_VALUE, "SECRET_RECEIPT_NAME"] {
                assert!(
                    !stderr.contains(secret)
                        && !String::from_utf8_lossy(&output.stdout).contains(secret)
                );
            }
            if args[0] == "--json" {
                let payload: serde_json::Value = serde_json::from_str(&stderr).unwrap();
                assert_eq!(
                    payload["details"]["config_scope"],
                    if system { "system" } else { "global" }
                );
                assert_eq!(
                    payload["details"]["schema_ledger"],
                    "configuration_schema_versions"
                );
                assert!(
                    payload["details"]["schema_reason"]
                        .as_str()
                        .unwrap()
                        .contains("newer")
                );
            }
        }
    }
}

#[test]
fn offline_and_local_storage_semantics_are_preserved() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_complete_local_storage_config();
    fixture.write_future_global_config();
    for output in [
        fixture.run(&fixture.repo, &["--offline", "pull"]),
        fixture.run(&fixture.repo, &["pull"]),
        fixture.run_with_complete_storage_env(&fixture.repo, &["pull"]),
    ] {
        assert!(!stderr_text(&output).contains("LBR-CONFIG-001"));
    }
    fs::create_dir_all(fixture.system_db.parent().unwrap()).unwrap();
    fs::copy(&fixture.global_db, &fixture.system_db).unwrap();
    let offline = fixture.run(&fixture.repo, &["--offline", "pull"]);
    assert!(!stderr_text(&offline).contains("LBR-CONFIG-001"));
    let online = fixture.run(&fixture.repo, &["pull"]);
    assert!(stderr_text(&online).contains("LBR-CONFIG-001"));
}

#[test]
fn unregistered_legacy_receipt_fails_closed() {
    use libra::internal::db::{DatabaseRole, schema};
    let barrier = schema::schema_manifest().configuration_barrier;
    for system in [false, true] {
        for variant in [
            "lower",
            "wrong-name",
            "null-name",
            "duplicate",
            "fake-barrier",
            "unpaired-barrier",
            "own-lower",
        ] {
            let fixture = CliFixture::new();
            fixture.init_repo();
            let (path, role) = if system {
                (&fixture.system_db, DatabaseRole::SystemConfig)
            } else {
                (&fixture.global_db, DatabaseRole::GlobalConfig)
            };
            configuration_base_fixture(path, role);
            fixture_sql(
                path,
                "CREATE TABLE schema_versions (version INTEGER, name TEXT, applied_at TEXT)",
            );
            let known = schema::migrations_for_role(DatabaseRole::Repository)
                .pop()
                .unwrap();
            let sql = match variant {
                "lower" => format!("INSERT INTO schema_versions VALUES ({}, '{}', 'fixture'), (1, 'SECRET_RECEIPT_NAME', 'fixture')", known.version, known.name),
                "wrong-name" => format!("INSERT INTO schema_versions VALUES ({}, 'SECRET_RECEIPT_NAME', 'fixture')", known.version),
                "null-name" => format!("INSERT INTO schema_versions VALUES ({}, NULL, 'fixture')", known.version),
                "duplicate" => format!("INSERT INTO schema_versions VALUES ({0}, '{1}', 'fixture'), ({0}, '{1}', 'fixture')", known.version, known.name),
                "fake-barrier" => format!("INSERT INTO schema_versions VALUES ({}, 'SECRET_RECEIPT_NAME', 'fixture')", barrier.version),
                "unpaired-barrier" => format!("DELETE FROM configuration_schema_versions; INSERT INTO schema_versions VALUES ({}, '{}', 'fixture')", barrier.version, barrier.name),
                _ => "INSERT INTO configuration_schema_versions VALUES (1, 'SECRET_RECEIPT_NAME', 'fixture')".into(),
            };
            fixture_sql(path, &sql);
            let before = fs::read(path).unwrap();
            let output = fixture.run(&fixture.repo, &["--json", "pull"]);
            let stderr = stderr_text(&output);
            assert!(
                stderr.contains("LBR-CONFIG-001")
                    && stderr.contains("unsupported migration receipt"),
                "{variant}: {stderr}"
            );
            assert!(!stderr.contains("SECRET_RECEIPT_NAME"));
            let rejected = fixture.run(
                &fixture.repo,
                &[
                    "config",
                    "set",
                    if system { "--system" } else { "--global" },
                    "test.no",
                    "forbidden",
                ],
            );
            assert!(!rejected.status.success());
            assert_eq!(before, fs::read(path).unwrap(), "{variant}");
        }
    }
}

#[test]
fn configuration_barrier_does_not_block_same_build() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    for (flag, path, role) in [
        (
            "--global",
            &fixture.global_db,
            libra::internal::db::DatabaseRole::GlobalConfig,
        ),
        (
            "--system",
            &fixture.system_db,
            libra::internal::db::DatabaseRole::SystemConfig,
        ),
    ] {
        configuration_base_fixture(path, role);
        fixture_sql(
            path,
            "CREATE TRIGGER reject_config BEFORE INSERT ON config_kv BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        );
        let before = fs::read(path).unwrap();
        let failed = fixture.run(
            &fixture.repo,
            &["config", "set", flag, "test.barrier", "forbidden"],
        );
        assert!(!failed.status.success());
        let stderr = stderr_text(&failed);
        assert!(
            stderr.contains("failed to set config 'test.barrier'") && stderr.contains("config_kv"),
            "{stderr}"
        );
        assert_eq!(
            before,
            fs::read(path).unwrap(),
            "failed explicit mutation must roll back the marker too"
        );
        fixture_sql(path, "DROP TRIGGER reject_config");
        let before = fs::read(path).unwrap();
        let missing = fixture.run(
            &fixture.repo,
            &["config", "--remove-section", flag, "absent"],
        );
        assert!(!missing.status.success() && stderr_text(&missing).contains("No such section"));
        assert_eq!(
            before,
            fs::read(path).unwrap(),
            "section validation failure must roll back its marker"
        );
        fixture.success(
            &fixture.repo,
            &["config", "set", flag, "test.barrier", "first"],
        );
        fixture.success(
            &fixture.repo,
            &["config", "set", flag, "test.barrier", "second"],
        );
        let output = fixture.success(&fixture.repo, &["config", "get", flag, "test.barrier"]);
        assert!(String::from_utf8_lossy(&output.stdout).contains("second"));
    }
    assert_preflight_allowed(&fixture);
}

#[test]
fn repository_receipt_keeps_global_config_values_readable() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    configuration_base_fixture(
        &fixture.global_db,
        libra::internal::db::DatabaseRole::GlobalConfig,
    );
    known_repository_receipts(&fixture.global_db);
    let before = fs::read(&fixture.global_db).unwrap();
    for args in [
        vec!["config", "get", "test.receipt"],
        vec!["config", "get", "--global", "test.receipt"],
    ] {
        let output = fixture.success(&fixture.repo, &args);
        assert!(String::from_utf8_lossy(&output.stdout).contains("preserved-value"));
    }
    assert_eq!(before, fs::read(&fixture.global_db).unwrap());
}

#[test]
fn read_paths_never_write_configuration_barrier() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    for (path, role, flag) in [
        (
            &fixture.global_db,
            libra::internal::db::DatabaseRole::GlobalConfig,
            "--global",
        ),
        (
            &fixture.system_db,
            libra::internal::db::DatabaseRole::SystemConfig,
            "--system",
        ),
    ] {
        configuration_base_fixture(path, role);
        fixture_sql(
            path,
            "INSERT INTO config_kv (key, value, encrypted) VALUES ('test.read', 'read-only', 0)",
        );
        let before = fs::read(path).unwrap();
        fixture.success(&fixture.repo, &["config", "get", flag, "test.read"]);
        fixture.success(&fixture.repo, &["config", "list", flag]);
        fixture.success(&fixture.repo, &["config", "get", "test.read"]);
        fixture.success(&fixture.repo, &["status"]);
        assert_preflight_allowed(&fixture);
        assert_eq!(before, fs::read(path).unwrap());
        let tables = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(configuration_tables(path));
        assert!(!tables.iter().any(|name| name == "schema_versions"));
    }
}

#[test]
fn config_policy_docs_are_synchronized() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for language in ["", "zh-CN/"] {
        for command in ["config", "pull", "push", "fetch", "clone", "cloud"] {
            let path = root.join(format!("docs/commands/{language}{command}.md"));
            let text = fs::read_to_string(&path).unwrap();
            for anchor in ["LBR-CONFIG-001", "2026090801", "barrier"] {
                assert!(text.contains(anchor), "{} missing {anchor}", path.display());
            }
        }
    }
    for path in ["COMPATIBILITY.md", "docs/error-codes.md"] {
        let text = fs::read_to_string(root.join(path)).unwrap();
        assert!(
            text.contains("LBR-CONFIG-001") && text.contains("configuration_schema_versions"),
            "{path}"
        );
    }
}

async fn raw_config_fixture(path: &Path) -> sea_orm::DatabaseConnection {
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    if !path.exists() {
        fs::File::create(path).expect("reserve fixture file");
    }
    let filename = path.to_path_buf();
    let mut options = sea_orm::ConnectOptions::new("sqlite://fixture");
    options
        .sqlx_logging(false)
        .map_sqlx_sqlite_pool_opts(|pool| pool.idle_timeout(None).max_lifetime(None))
        .map_sqlx_sqlite_opts(move |opts| opts.filename(&filename).create_if_missing(false));
    sea_orm::Database::connect(options)
        .await
        .expect("open literal fixture")
}

async fn configuration_tables(path: &Path) -> Vec<String> {
    let conn = raw_config_fixture(path).await;
    let names = conn.query_all_raw(Statement::from_string(conn.get_database_backend(),
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"))
        .await.expect("inspect fixture topology").into_iter()
        .map(|row| row.try_get_by_index(0).expect("table name")).collect();
    conn.close().await.expect("close topology reader");
    names
}

#[tokio::test]
#[serial_test::serial(env, cwd)]
async fn config_cascade_uses_explicit_role() {
    use libra::{
        internal::{
            config::{
                ConfigKv, LocalIdentityTarget, read_cascaded_config_value,
                read_cascaded_config_value_decrypted, read_cascaded_config_value_fresh_conn,
                read_cascaded_config_value_strict, resolve_env_for_target,
            },
            db::{DatabaseRole, create_database_for_role, schema::latest_schema_version_for_role},
        },
        utils::test::ConfigDbFixture,
    };
    let fixture = ConfigDbFixture::new().expect("isolate every configuration path");
    let _cwd = libra::utils::test::ChangeDirGuard::new(fixture.root());
    let none = LocalIdentityTarget::None;
    assert_eq!(
        read_cascaded_config_value_strict(none, "test.role")
            .await
            .expect("absent scopes"),
        None
    );
    assert!(!fixture.global_db().exists() && !fixture.system_db().exists());
    // An empty pre-ledger SQLite file is still an absent configuration scope,
    // not a reason to bootstrap schema on a read.
    let empty = raw_config_fixture(fixture.global_db()).await;
    empty.close().await.expect("close empty fixture");
    let empty_before = fs::read(fixture.global_db()).expect("empty snapshot");
    assert_eq!(
        read_cascaded_config_value_strict(none, "test.role")
            .await
            .expect("empty scope"),
        None
    );
    assert_eq!(
        fs::read(fixture.global_db()).expect("empty after read"),
        empty_before
    );
    for (path, role, value) in [
        (fixture.global_db(), DatabaseRole::GlobalConfig, "global"),
        (fixture.system_db(), DatabaseRole::SystemConfig, "system"),
    ] {
        fs::create_dir_all(path.parent().expect("parent")).expect("create scope directory");
        let conn = if path.exists() {
            libra::internal::db::upgrade_database_schema_for_role(path, role)
                .await
                .expect("empty scope bootstrap");
            raw_config_fixture(path).await
        } else {
            create_database_for_role(path.to_str().expect("fixture path"), role)
                .await
                .expect("role bootstrap")
        };
        ConfigKv::set_with_conn(&conn, "test.role", value, false)
            .await
            .expect("scope value");
        ConfigKv::set_with_conn(&conn, "test.system", "system-only", false)
            .await
            .expect("fallback value");
        if role == DatabaseRole::GlobalConfig {
            ConfigKv::unset_with_conn(&conn, "test.system")
                .await
                .expect("global has no fallback key");
            ConfigKv::set_with_conn(
                &conn,
                "vault.env.LIBRA_MIG03_FIXTURE_ONLY",
                "credential-fixture",
                false,
            )
            .await
            .expect("credential fixture");
        }
        conn.close().await.expect("close fixture writer");
    }
    let snapshots: Vec<_> = [fixture.global_db(), fixture.system_db()]
        .into_iter()
        .map(|path| fs::read(path).expect("before read"))
        .collect();
    assert_eq!(
        read_cascaded_config_value_strict(none, "TEST.ROLE")
            .await
            .expect("case insensitive global"),
        Some("global".into())
    );
    assert_eq!(
        read_cascaded_config_value_strict(none, "test.system")
            .await
            .expect("system fallback"),
        Some("system-only".into())
    );
    assert_eq!(
        read_cascaded_config_value(none, "test.role")
            .await
            .expect("exact global"),
        Some("global".into())
    );
    assert_eq!(
        read_cascaded_config_value_decrypted(none, "test.role")
            .await
            .expect("decrypted reader"),
        Some("global".into())
    );
    assert_eq!(
        resolve_env_for_target("LIBRA_MIG03_FIXTURE_ONLY", none)
            .await
            .expect("vault reader"),
        Some("credential-fixture".into())
    );
    assert_eq!(
        read_cascaded_config_value_fresh_conn("test.role").await,
        Some("global".into())
    );
    let local_path = fixture.root().join("local.db");
    let local = create_database_for_role(
        local_path.to_str().expect("local path"),
        DatabaseRole::Repository,
    )
    .await
    .expect("repository fixture");
    ConfigKv::set_with_conn(&local, "test.role", "local", false)
        .await
        .expect("local override");
    assert_eq!(
        read_cascaded_config_value_strict(
            LocalIdentityTarget::ExplicitDb(&local_path),
            "test.role"
        )
        .await
        .expect("local precedence"),
        Some("local".into())
    );
    local.close().await.expect("close repository fixture");
    for (path, before) in [fixture.global_db(), fixture.system_db()]
        .into_iter()
        .zip(snapshots)
    {
        assert_eq!(
            fs::read(path).expect("after read"),
            before,
            "cascade must not migrate {path:?}"
        );
    }
    let conn = raw_config_fixture(fixture.global_db()).await;
    let latest = latest_schema_version_for_role(DatabaseRole::GlobalConfig)
        .expect("manifest")
        .expect("latest");
    conn.execute_raw(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO configuration_schema_versions VALUES (?, 'future', 'fixture')",
        [(latest + 1).into()],
    ))
    .await
    .expect("future config fixture");
    let before = fs::read(fixture.global_db()).expect("future snapshot");
    assert_eq!(
        read_cascaded_config_value_strict(none, "test.role")
            .await
            .expect("local cascade skips unsupported Global defaults"),
        Some("system".into()),
        "true Global future must not return its value; supported System fallback remains available to local commands"
    );
    assert_eq!(
        read_cascaded_config_value_fresh_conn("test.role").await,
        Some("global".into()),
        "fresh optional plaintext reader retains its query-based policy"
    );
    assert_eq!(
        fs::read(fixture.global_db()).expect("future after read"),
        before
    );
    conn.execute_unprepared("DROP TABLE configuration_schema_versions; DROP TABLE config_kv; INSERT INTO config (configuration, name, key, value) VALUES ('test', NULL, 'role', 'legacy-global')")
        .await.expect("legacy-only fixture");
    let before = fs::read(fixture.global_db()).expect("legacy snapshot");
    assert_eq!(
        read_cascaded_config_value_strict(none, "TEST.ROLE")
            .await
            .expect("legacy table without bootstrap"),
        Some("legacy-global".into())
    );
    assert_eq!(
        read_cascaded_config_value(none, "test.role")
            .await
            .expect("no modern rows"),
        None
    );
    assert_eq!(
        fs::read(fixture.global_db()).expect("legacy after read"),
        before
    );
    let system = raw_config_fixture(fixture.system_db()).await;
    system.execute_unprepared("DROP TABLE configuration_schema_versions; DROP TABLE config_kv; INSERT INTO config (configuration, name, key, value) VALUES ('test', NULL, 'system', 'legacy-system')").await.expect("legacy system-only fixture");
    let system_before = fs::read(fixture.system_db()).expect("system legacy snapshot");
    assert_eq!(
        read_cascaded_config_value_strict(none, "test.system")
            .await
            .expect("system legacy fallback"),
        Some("legacy-system".into())
    );
    assert_eq!(
        fs::read(fixture.system_db()).expect("system after read"),
        system_before
    );
    system.close().await.expect("close system fixture");
    conn.execute_unprepared("CREATE TABLE config_kv (id INTEGER PRIMARY KEY, wrong_key TEXT)")
        .await
        .expect("malformed table fixture");
    assert!(
        read_cascaded_config_value_strict(none, "test.role")
            .await
            .is_err(),
        "malformed modern table must not silently fall back to legacy"
    );
    conn.execute_unprepared("DROP TABLE config_kv; CREATE TABLE configuration_schema_versions (version INTEGER PRIMARY KEY, name TEXT, applied_at TEXT)").await.expect("missing receipted table fixture");
    conn.execute_raw(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO configuration_schema_versions VALUES (?, 'configuration_base', 'fixture')",
        [latest.into()],
    ))
    .await
    .expect("current receipt");
    let error = read_cascaded_config_value_strict(none, "test.role")
        .await
        .expect_err("missing required table is corruption");
    assert!(format!("{error:#}").contains("missing its required config_kv table"));
    conn.close().await.expect("close fixture");
}

#[test]
fn config_role_bootstrap_writes_configuration_state_only() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    let repo_db = fixture.repo.join(".libra/libra.db");
    let repo_before = fs::read(&repo_db).expect("repository snapshot");
    for (flag, path) in [
        ("--global", &fixture.global_db),
        ("--system", &fixture.system_db),
    ] {
        fixture.success(
            &fixture.root,
            &["config", "set", flag, "test.role", "scope-fixture"],
        );
        let runtime = tokio::runtime::Runtime::new().expect("fixture runtime");
        assert_eq!(
            runtime.block_on(configuration_tables(path)),
            [
                "config",
                "config_kv",
                "configuration_schema_versions",
                "schema_versions"
            ]
        );
    }
    assert_eq!(
        fs::read(repo_db).expect("repository after scoped writes"),
        repo_before
    );

    // Exercise the credential resolver and fresh reader in an isolated child.
    let runtime = tokio::runtime::Runtime::new().expect("fixture runtime");
    runtime.block_on(async {
        let conn = raw_config_fixture(&fixture.global_db).await;
        libra::internal::config::ConfigKv::set_with_conn(
            &conn,
            "vault.env.LIBRA_STORAGE_TYPE",
            "local",
            false,
        )
        .await
        .expect("storage config");
        conn.close().await.expect("close fixture");
    });
    let before = fs::read(&fixture.global_db).expect("before command reads");
    fixture.success(&fixture.repo, &["status"]);
    let check = fixture.run(
        &fixture.repo,
        &[
            "check-ignore",
            "--no-index",
            "--non-matching",
            "--verbose",
            "fixture-untracked",
        ],
    );
    assert!(
        matches!(check.status.code(), Some(0 | 1)),
        "{}",
        stderr_text(&check)
    );
    assert!(String::from_utf8_lossy(&check.stdout).contains("fixture-untracked"));
    assert_eq!(
        fs::read(&fixture.global_db).expect("after command reads"),
        before
    );

    // A true configuration-owned future still rejects a scoped writer.
    fixture.write_future_global_config();
    let before = fs::read(&fixture.global_db).expect("legacy future snapshot");
    let rejected = fixture.run(
        &fixture.root,
        &["config", "set", "--global", "test.role", "forbidden"],
    );
    assert!(!rejected.status.success());
    assert!(stderr_text(&rejected).contains("newer"));
    assert_eq!(
        fs::read(&fixture.global_db).expect("after rejected writer"),
        before
    );

    #[cfg(unix)]
    {
        let mut literal = CliFixture::new();
        literal.global_db = literal.home.join("file:config?mode=memory&cache=shared.db");
        literal.success(
            &literal.root,
            &["config", "set", "--global", "test.literal", "literal-path"],
        );
        let before = fs::read(&literal.global_db).expect("literal path was created");
        let output = literal.success(
            &literal.root,
            &["config", "get", "--global", "test.literal"],
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("literal-path"));
        assert_eq!(
            fs::read(&literal.global_db).expect("literal path retained"),
            before
        );
        assert!(
            !literal.home.join("file:config").exists(),
            "URI options must not redirect writes"
        );
    }
}

#[test]
fn config_callsites_do_not_use_generic_schema_wrapper() {
    use syn::visit::Visit;
    #[derive(Default)]
    struct Calls(Vec<String>);
    impl<'ast> Visit<'ast> for Calls {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref()
                && let Some(segment) = path.path.segments.last()
            {
                self.0.push(segment.ident.to_string());
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    fn assert_scoped(source: &str, names: &[&str]) {
        let file = syn::parse_file(source).expect("parse production source");
        let mut found = 0;
        for item in file.items {
            let functions: Vec<_> = match item {
                syn::Item::Fn(function) => vec![(function.sig.ident, function.block)],
                syn::Item::Impl(implementation) => implementation
                    .items
                    .into_iter()
                    .filter_map(|item| {
                        if let syn::ImplItem::Fn(function) = item {
                            Some((function.sig.ident, Box::new(function.block)))
                        } else {
                            None
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            for (name, block) in functions {
                if names.contains(&name.to_string().as_str()) {
                    found += 1;
                    let mut calls = Calls::default();
                    calls.visit_block(&block);
                    for generic in [
                        "create_database",
                        "establish_connection",
                        "establish_connection_with_busy_timeout",
                        "get_db_conn_instance_for_path",
                        "open_connection_without_schema_management",
                    ] {
                        assert!(
                            !calls.0.iter().any(|call| call == generic),
                            "{name} bypasses scoped API via {generic}"
                        );
                    }
                }
            }
        }
        assert_eq!(
            found,
            names.len(),
            "guard must still cover every named callsite"
        );
    }
    assert_scoped(
        include_str!("../../src/command/config.rs"),
        &["ensure_config_exists", "get_or_create_cached_connection"],
    );
    let readers = include_str!("../../src/internal/config.rs");
    assert_scoped(
        readers,
        &[
            "read_config_keys_by_prefix_from_db_path",
            "read_subsection_entries_from_db_path",
            "read_config_entry_from_db_path",
            "read_config_entry_from_db_path_case_insensitive",
            "read_cascaded_fresh_conn_at",
        ],
    );
    assert_scoped(
        include_str!("../../src/utils/client_storage.rs"),
        &["read_config_env_value"],
    );
    let compact = readers.split_whitespace().collect::<String>();
    assert!(
        compact.contains("DatabaseRole::Repository=>get_db_conn_instance_for_path(db_path)"),
        "only the explicit repository branch may use its migrating cache"
    );
    let schema = include_str!("../../src/internal/db/schema.rs")
        .split_whitespace()
        .collect::<String>();
    assert!(schema.contains(".filename(&filename).read_only(read_only).create_if_missing(false)"));
    assert!(schema.contains("open_literal_connection(db_path,busy_timeout,role,true).await"));
    assert!(schema.contains("std::path::absolute(db_path)"));
}

struct CliFixture {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    global_db: PathBuf,
    system_db: PathBuf,
    future_schema_version: i64,
    latest_schema_version: i64,
}

impl CliFixture {
    fn new() -> Self {
        let temp = tempdir().expect("create tempdir");
        let root = temp.path().to_path_buf();
        let home = root.join("home");
        let repo = root.join("repo");
        let global_db = home.join(".libra").join("config.db");
        let system_db = root.join("system").join("config.db");
        fs::create_dir_all(&home).expect("create isolated home");
        let latest_schema_version = libra::internal::db::schema::latest_schema_version_for_role(
            libra::internal::db::DatabaseRole::GlobalConfig,
        )
        .expect("read latest schema version")
        .expect("built-in migrations should have a latest schema version");
        Self {
            _temp: temp,
            root,
            home,
            repo,
            global_db,
            system_db,
            future_schema_version: latest_schema_version + 1,
            latest_schema_version,
        }
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let config_home = self.home.join(".config");
        fs::create_dir_all(&config_home).expect("create isolated config dir");
        fs::create_dir_all(self.global_db.parent().expect("global db parent"))
            .expect("create global config dir");
        fs::create_dir_all(self.system_db.parent().expect("system db parent"))
            .expect("create system config dir");

        let mut command = Command::new(env!("CARGO_BIN_EXE_libra"));
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("LIBRA_CONFIG_GLOBAL_DB", &self.global_db)
            .env("LIBRA_CONFIG_SYSTEM_DB", &self.system_db)
            .env("LIBRA_TEST", "1")
            .env("LANG", "C")
            .env("LC_ALL", "C");
        if let Some(profile_file) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile_file);
        }
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().expect("spawn libra")
    }

    fn run_env(&self, cwd: &Path, args: &[&str], key: &str, value: &str) -> Output {
        self.command(cwd, args)
            .env(key, value)
            .output()
            .expect("spawn libra with env")
    }

    fn run_envs(&self, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut command = self.command(cwd, args);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("spawn libra with envs")
    }

    fn run_with_complete_storage_env(&self, cwd: &Path, args: &[&str]) -> Output {
        self.run_envs(
            cwd,
            args,
            &[
                ("LIBRA_STORAGE_TYPE", "r2"),
                ("LIBRA_STORAGE_BUCKET", "schema-future-test"),
                ("LIBRA_STORAGE_ENDPOINT", "http://127.0.0.1:1"),
                ("LIBRA_STORAGE_REGION", "auto"),
                ("LIBRA_STORAGE_ACCESS_KEY", "test-access-key"),
                ("LIBRA_STORAGE_SECRET_KEY", ENV_SECRET_VALUE),
                ("LIBRA_STORAGE_ALLOW_HTTP", "true"),
                ("LIBRA_STORAGE_THRESHOLD", "1048576"),
                ("LIBRA_STORAGE_CACHE_SIZE", "2097152"),
            ],
        )
    }

    fn write_complete_local_storage_config(&self) {
        for (key, value) in [
            ("vault.env.LIBRA_STORAGE_TYPE", "r2"),
            ("vault.env.LIBRA_STORAGE_BUCKET", "schema-future-test"),
            ("vault.env.LIBRA_STORAGE_ENDPOINT", "http://127.0.0.1:1"),
            ("vault.env.LIBRA_STORAGE_REGION", "auto"),
            ("vault.env.LIBRA_STORAGE_ACCESS_KEY", "test-access-key"),
            ("vault.env.LIBRA_STORAGE_SECRET_KEY", ENV_SECRET_VALUE),
            ("vault.env.LIBRA_STORAGE_ALLOW_HTTP", "true"),
            ("vault.env.LIBRA_STORAGE_THRESHOLD", "1048576"),
            ("vault.env.LIBRA_STORAGE_CACHE_SIZE", "2097152"),
        ] {
            self.success(&self.repo, &["config", "set", key, value]);
        }
    }

    fn success(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = self.run(cwd, args);
        assert_success(args, &output);
        output
    }

    fn init_repo(&self) {
        fs::create_dir_all(&self.repo).expect("create repo dir");
        self.success(
            &self.root,
            &[
                "init",
                "--vault",
                "false",
                self.repo.to_str().expect("utf8 repo"),
            ],
        );
    }

    fn write_future_global_config(&self) {
        if self.global_db.exists() {
            fs::remove_file(&self.global_db).expect("remove previous global config db");
        }
        fs::create_dir_all(self.global_db.parent().expect("global db parent"))
            .expect("create global config dir");
        let db_path = self.global_db.to_str().expect("utf8 global db");
        let runtime = tokio::runtime::Runtime::new().expect("create tokio runtime");
        runtime.block_on(async {
            let conn = libra::internal::db::create_database_for_role(db_path, libra::internal::db::DatabaseRole::GlobalConfig)
                .await
                .expect("create global config db");
            let backend = conn.get_database_backend();
            conn.execute_raw(Statement::from_sql_and_values(
                backend,
                "DELETE FROM configuration_schema_versions",
                [],
            ))
            .await
            .expect("clear schema versions");
            conn.execute_raw(Statement::from_sql_and_values(
                backend,
                "INSERT INTO configuration_schema_versions (version, name, applied_at) VALUES (?, ?, ?)",
                [
                    self.future_schema_version.into(),
                    "future_schema_for_test".into(),
                    "2026-07-09T00:00:00Z".into(),
                ],
            ))
            .await
            .expect("insert future schema version");
            conn.execute_raw(Statement::from_sql_and_values(
                backend,
                "INSERT INTO config_kv (`key`, `value`, `encrypted`) VALUES (?, ?, 0)",
                [
                    "vault.env.LIBRA_STORAGE_SECRET_KEY".into(),
                    SECRET_VALUE.into(),
                    0.into(),
                ],
            ))
            .await
            .expect("insert secret-like value");
            conn.close().await.expect("close global config db");
        });
    }
}

fn assert_success(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "{} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_not_success(args: &[&str], output: &Output) {
    assert!(
        !output.status.success(),
        "{} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is utf8")
}

fn assert_schema_future_diagnostic(fixture: &CliFixture, stderr: &str) {
    assert!(
        stderr.contains("LBR-CONFIG-001") || stderr.contains("warning:"),
        "expected config error code or warning, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("global config database schema is newer"),
        "missing global config schema diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))),
        "missing binary version:\n{stderr}"
    );
    assert!(
        stderr.contains(&fixture.global_db.display().to_string()),
        "missing global config db path:\n{stderr}"
    );
    assert!(
        stderr.contains(&fixture.future_schema_version.to_string()),
        "missing future schema version:\n{stderr}"
    );
    assert!(
        stderr.contains(&fixture.latest_schema_version.to_string()),
        "missing latest supported schema version:\n{stderr}"
    );
    assert!(
        stderr.contains(INSTALL_COMMAND),
        "missing install command:\n{stderr}"
    );
    assert!(
        !stderr.contains(SECRET_VALUE),
        "diagnostic leaked secret-like config value:\n{stderr}"
    );
}

#[test]
fn pull_fails_closed_when_global_config_schema_is_future() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run(&fixture.repo, &["pull"]);

    assert_not_success(&["pull"], &output);
    let stderr = stderr_text(&output);
    assert_schema_future_diagnostic(&fixture, &stderr);
    assert!(
        stderr.contains("`libra pull` requires global storage config"),
        "missing fail-closed command context:\n{stderr}"
    );
    assert!(
        stderr.contains("use --offline or LIBRA_READ_POLICY=offline/local"),
        "missing explicit downgrade escape hatch:\n{stderr}"
    );
}

#[test]
fn remote_and_cloud_commands_fail_closed_when_global_config_schema_is_future() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let clone_source = "https://example.invalid/schema-future.git";
    let cases = [
        ("fetch", fixture.repo.as_path(), vec!["fetch"]),
        ("push", fixture.repo.as_path(), vec!["push"]),
        ("cloud", fixture.repo.as_path(), vec!["cloud", "status"]),
        (
            "clone",
            fixture.root.as_path(),
            vec!["clone", clone_source, "copy"],
        ),
    ];

    for (command_name, cwd, args) in cases {
        let output = fixture.run(cwd, &args);
        assert_not_success(&args, &output);
        let stderr = stderr_text(&output);
        assert_schema_future_diagnostic(&fixture, &stderr);
        assert!(
            stderr.contains(&format!(
                "`libra {command_name}` requires global storage config"
            )),
            "missing fail-closed context for {command_name}:\n{stderr}"
        );
    }
}

#[test]
fn offline_policy_warns_and_allows_pull_to_reach_command() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run(&fixture.repo, &["--offline", "pull"]);
    let stderr = stderr_text(&output);

    assert!(
        !stderr.contains("LBR-CONFIG-001"),
        "--offline should not fail at global config schema guard:\n{stderr}"
    );
    assert_schema_future_diagnostic(&fixture, &stderr);
}

#[test]
fn env_offline_policy_warns_and_allows_pull_to_reach_command() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run_env(&fixture.repo, &["pull"], "LIBRA_READ_POLICY", "offline");
    let stderr = stderr_text(&output);

    assert!(
        !stderr.contains("LBR-CONFIG-001"),
        "LIBRA_READ_POLICY=offline should not fail at schema guard:\n{stderr}"
    );
    assert_schema_future_diagnostic(&fixture, &stderr);
}

#[test]
fn env_local_policy_warns_and_allows_pull_to_reach_command() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run_env(&fixture.repo, &["pull"], "LIBRA_READ_POLICY", "local");
    let stderr = stderr_text(&output);

    assert!(
        !stderr.contains("LBR-CONFIG-001"),
        "LIBRA_READ_POLICY=local should not fail at schema guard:\n{stderr}"
    );
    assert_schema_future_diagnostic(&fixture, &stderr);
}

#[test]
fn complete_process_env_storage_config_does_not_fail_schema_guard() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run_with_complete_storage_env(&fixture.repo, &["pull"]);
    let stderr = stderr_text(&output);

    assert!(
        !stderr.contains("LBR-CONFIG-001"),
        "complete LIBRA_STORAGE_* env should make global config unnecessary:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "process or repo-local configuration makes global storage config unnecessary"
        ),
        "missing env-override diagnostic:\n{stderr}"
    );
    assert_schema_future_diagnostic(&fixture, &stderr);
    assert!(
        !stderr.contains(ENV_SECRET_VALUE),
        "diagnostic leaked process env storage secret:\n{stderr}"
    );
}

#[test]
fn complete_repo_local_storage_config_does_not_fail_schema_guard() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_complete_local_storage_config();
    fixture.write_future_global_config();

    let output = fixture.run(&fixture.repo, &["pull"]);
    let stderr = stderr_text(&output);

    assert!(
        !stderr.contains("LBR-CONFIG-001"),
        "complete repo-local vault.env.LIBRA_STORAGE_* config should make global config unnecessary:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "process or repo-local configuration makes global storage config unnecessary"
        ),
        "missing local-config override diagnostic:\n{stderr}"
    );
    assert_schema_future_diagnostic(&fixture, &stderr);
    assert!(
        !stderr.contains(ENV_SECRET_VALUE),
        "diagnostic leaked repo-local storage secret:\n{stderr}"
    );
}

#[test]
fn cloud_storage_env_still_fails_when_d1_config_would_read_global() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run_with_complete_storage_env(&fixture.repo, &["cloud", "status"]);
    let stderr = stderr_text(&output);

    assert_not_success(&["cloud", "status"], &output);
    assert_schema_future_diagnostic(&fixture, &stderr);
    assert!(
        stderr.contains("`libra cloud` requires global storage config"),
        "cloud must fail closed when D1 config would fall through to global:\n{stderr}"
    );
    assert!(
        !stderr.contains(ENV_SECRET_VALUE),
        "diagnostic leaked process env storage secret:\n{stderr}"
    );
}

#[test]
fn local_command_warns_once_and_continues() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.success(&fixture.repo, &["status", "--short"]);
    let stderr = stderr_text(&output);

    assert_schema_future_diagnostic(&fixture, &stderr);
    let count = stderr
        .matches("global config database schema is newer")
        .count();
    assert_eq!(count, 1, "schema warning should be deduplicated:\n{stderr}");
}

/// End-to-end: a local `commit` (which reads config through both the strict
/// `commit.gpgSign` cascade and the error-swallowing non-strict
/// `commit.cleanup` read) succeeds against a future-schema global store with
/// one deduplicated warning (P0-12). The non-strict `global_config_value`
/// carve-out itself is pinned directly by the
/// `internal::config::tests::global_config_value_skips_future_schema_and_keeps_other_errors`
/// unit test — this test guards the command-level outcome.
#[test]
fn non_strict_cascade_local_command_warns_once_and_continues() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.success(&fixture.repo, &["config", "set", "user.name", "Schema T"]);
    fixture.success(&fixture.repo, &["config", "set", "user.email", "t@t.io"]);
    fixture.write_future_global_config();

    let args = ["commit", "--allow-empty", "-m", "future schema commit"];
    let output = fixture.run(&fixture.repo, &args);
    assert_success(&args, &output);
    let stderr = stderr_text(&output);

    assert_schema_future_diagnostic(&fixture, &stderr);
    let count = stderr
        .matches("global config database schema is newer")
        .count();
    assert_eq!(count, 1, "schema warning should be deduplicated:\n{stderr}");
}

/// A global config store that is unreadable for any reason OTHER than a
/// future schema keeps the original fail-closed `LBR-IO-001` contract:
/// `status --short` pins the strict cascade, and `commit` pins the
/// command-level outcome (its failure travels through the strict
/// `commit.gpgSign` read; the non-strict corruption path is pinned by the
/// `internal::config` unit test). The schema carve-out must not swallow
/// corruption.
#[test]
fn corrupt_global_config_store_keeps_io_error_for_local_commands() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.success(&fixture.repo, &["config", "set", "user.name", "Schema T"]);
    fixture.success(&fixture.repo, &["config", "set", "user.email", "t@t.io"]);
    fs::create_dir_all(fixture.global_db.parent().expect("global db parent"))
        .expect("create global config dir");
    fs::write(&fixture.global_db, b"this is not a sqlite database")
        .expect("write corrupt global config db");

    // Strict cascade (status.* defaults).
    let status = fixture.run(&fixture.repo, &["status", "--short"]);
    assert_not_success(&["status", "--short"], &status);
    let stderr = stderr_text(&status);
    assert!(stderr.contains("LBR-IO-001"), "stderr was: {stderr}");
    assert!(
        !stderr.contains("global config database schema is newer"),
        "corruption must not be misclassified as a future schema:\n{stderr}"
    );

    // Non-strict cascade (commit.cleanup and friends).
    let commit_args = ["commit", "--allow-empty", "-m", "corrupt global"];
    let commit = fixture.run(&fixture.repo, &commit_args);
    assert_not_success(&commit_args, &commit);
    let stderr = stderr_text(&commit);
    assert!(stderr.contains("LBR-IO-001"), "stderr was: {stderr}");
}

#[test]
fn json_error_reports_config_schema_future_details() {
    let fixture = CliFixture::new();
    fixture.init_repo();
    fixture.write_future_global_config();

    let output = fixture.run(&fixture.repo, &["--json", "pull"]);

    assert_not_success(&["--json", "pull"], &output);
    assert!(
        output.stdout.is_empty(),
        "JSON errors must stay on stderr, got stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = stderr_text(&output);
    assert!(
        !stderr.contains(SECRET_VALUE),
        "JSON diagnostic leaked secret-like config value:\n{stderr}"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&stderr).expect("stderr should be a JSON error envelope");
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error_code"], "LBR-CONFIG-001");
    assert_eq!(payload["category"], "config");
    assert_eq!(payload["exit_code"], 128);
    assert_eq!(payload["details"]["command"], "pull");
    assert_eq!(
        payload["details"]["config_database"],
        fixture.global_db.display().to_string()
    );
    assert_eq!(
        payload["details"]["config_schema_version"],
        fixture.future_schema_version
    );
    assert_eq!(
        payload["details"]["latest_supported_schema_version"],
        fixture.latest_schema_version.to_string()
    );
    assert_eq!(payload["details"]["install_command"], INSTALL_COMMAND);
}
