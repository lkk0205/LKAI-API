//! 数据兼容策略的端到端验证：从 0.4.0 开始升级必须保留用户数据。
//!
//! 这些用例走 `Database::new_with_path`（桌面端与 headless 共用的入口），
//! 因此覆盖到清理残留、迁移前备份、行尾校验修复、建库、跑迁移的完整启动链路。

use std::path::{Path, PathBuf};

use lkapi_lib::db::Database;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lkapi-db-reset-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 迁移集里的最大版本号（即“库是否跑到了当前 schema”）。
fn expected_version() -> i64 {
    sqlx::migrate!("./migrations")
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(0)
}

async fn applied_version(db: &Database) -> i64 {
    let max: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    max.unwrap_or(0)
}

/// 全新目录：直接建出当前 schema。
#[tokio::test]
async fn fresh_directory_is_initialised_to_latest_schema() {
    let dir = temp_dir("fresh");
    let db = Database::new_with_path(&dir).await;

    assert_eq!(applied_version(&db).await, expected_version());
    // 老版本遗留的库文件名不应出现在新装环境里
    assert!(!dir.join("waliapi.db").exists());

    db.pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 旧产品名数据库会被清掉；本产品的迁移前备份与隔离文件必须保留。
#[tokio::test]
async fn legacy_artifacts_are_purged_on_startup() {
    let dir = temp_dir("purge");
    for name in [
        "waliapi.db",
        "waliapi.db.pre-upgrade-20260101-000000",
        "lkapi.db.pre-upgrade-20260101-000000",
        "lkapi.db.broken-20260101-000000",
        "settings.json",
    ] {
        std::fs::write(dir.join(name), b"legacy").unwrap();
    }

    let db = Database::new_with_path(&dir).await;
    assert_eq!(applied_version(&db).await, expected_version());

    for name in [
        "waliapi.db",
        "waliapi.db.pre-upgrade-20260101-000000",
    ] {
        assert!(
            !dir.join(name).exists(),
            "旧产品名数据应被清理，但仍存在: {name}"
        );
    }
    for name in [
        "lkapi.db.pre-upgrade-20260101-000000",
        "lkapi.db.broken-20260101-000000",
        "settings.json",
    ] {
        assert!(
            dir.join(name).exists(),
            "备份/隔离/配置文件必须保留，但已被删除: {name}"
        );
    }

    db.pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 无法按当前 schema 打开的旧库：隔离保留原文件后重建，绝不静默删除。
#[tokio::test]
async fn incompatible_database_is_quarantined() {
    let dir = temp_dir("replace");
    let db_path = dir.join("lkapi.db");

    // 模拟旧版：迁移记录版本/checksum 与当前迁移集对不上，且缺少业务表。
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version INTEGER PRIMARY KEY, description TEXT NOT NULL, \
             installed_on TEXT NOT NULL, success INTEGER NOT NULL, checksum BLOB NOT NULL, \
             execution_time INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) \
             VALUES (1, 'legacy', '2026-01-01T00:00:00Z', 1, X'00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE TABLE old_only_table (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    let db = Database::new_with_path(&dir).await;
    assert_eq!(applied_version(&db).await, expected_version(), "应重建到最新 schema");

    // 旧表不应残留：重建意味着整库换新，而不是增量升级。
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'old_only_table'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(exists, Some(0), "旧表不应存在于重建后的库里");

    // 原库必须以 `lkapi.db.broken-*` 形式留在目录里，供手工恢复。
    let quarantined: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("lkapi.db.broken-"))
        .collect();
    assert!(
        !quarantined.is_empty(),
        "原库应被隔离保留，而不是删除"
    );

    db.pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 0.4.0 的老库（CRLF checksum）升级：业务数据必须原样保留，不触发重建。
#[tokio::test]
async fn crlf_legacy_database_upgrades_without_data_loss() {
    use sha2::Digest;

    let dir = temp_dir("crlf-upgrade");

    {
        let db = Database::new_with_path(&dir).await;
        // 写一条业务数据，升级后必须还在。
        sqlx::query("CREATE TABLE IF NOT EXISTS _lkapi_upgrade_marker (id INTEGER PRIMARY KEY, note TEXT)")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO _lkapi_upgrade_marker (note) VALUES ('keep-me')")
            .execute(&db.pool)
            .await
            .unwrap();

        // 模拟 0.4.0 安装包：所有迁移 checksum 都是 CRLF 变体。
        let migrator = sqlx::migrate!("./migrations");
        let mut tx = db.pool.begin().await.unwrap();
        for migration in migrator.iter() {
            let crlf = migration.sql.replace("\r\n", "\n").replace('\n', "\r\n");
            let checksum = sha2::Sha384::digest(crlf.as_bytes()).to_vec();
            sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
                .bind(checksum)
                .bind(migration.version)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        db.pool.close().await;
    }

    // 模拟升级后的首次启动
    let db = Database::new_with_path(&dir).await;
    assert_eq!(applied_version(&db).await, expected_version(), "不应重建数据库");

    let note: String = sqlx::query_scalar("SELECT note FROM _lkapi_upgrade_marker LIMIT 1")
        .fetch_one(&db.pool)
        .await
        .expect("升级后业务数据必须保留");
    assert_eq!(note, "keep-me");

    let backups = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("lkapi.db.pre-upgrade-")
        })
        .count();
    assert_eq!(backups, 0, "无 schema 变更时不应产生备份文件");

    db.pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 同一目录重复启动是幂等的（自动更新后的首次启动会走到这条路径）。
#[tokio::test]
async fn restart_on_existing_database_is_idempotent() {
    let dir = temp_dir("restart");
    {
        let db = Database::new_with_path(&dir).await;
        assert_eq!(applied_version(&db).await, expected_version());
        db.pool.close().await;
    }

    let db = Database::new_with_path(&dir).await;
    assert_eq!(applied_version(&db).await, expected_version());
    db.pool.close().await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(dead_code)]
fn assert_exists(path: &Path) {
    assert!(path.exists(), "{} 应存在", path.display());
}
