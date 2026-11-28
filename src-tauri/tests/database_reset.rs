//! 数据兼容策略的端到端验证：老版本数据一律丢弃，不与新 schema 共存。
//!
//! 这些用例走 `Database::new_with_path`（桌面端与 headless 共用的入口），
//! 因此覆盖到清理残留、建库、跑迁移、补建索引的完整启动链路。

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

/// 目录里残留的旧库/旧备份会被直接清掉，不影响新库建立。
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
        "lkapi.db.pre-upgrade-20260101-000000",
        "lkapi.db.broken-20260101-000000",
    ] {
        assert!(
            !dir.join(name).exists(),
            "旧数据应被清理，但仍存在: {name}"
        );
    }
    assert!(
        dir.join("settings.json").exists(),
        "配置文件不属于旧数据库，必须保留"
    );

    db.pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 无法按当前 schema 打开的旧库：整库丢弃重建，而不是尝试修补或备份。
#[tokio::test]
async fn incompatible_database_is_replaced() {
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
