pub mod models;
pub mod repository;

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tauri::{AppHandle, Manager};

/// 数据库文件名。
const DB_FILE_NAME: &str = "lkapi.db";

/// 旧产品名留下的数据库文件名（`waliapi.db` 等）。
///
/// 本应用明确**不兼容任何历史版本的用户数据**，启动时会清掉这些遗留文件。
const LEGACY_DB_STEMS: &[&str] = &["waliapi.db", "xapi.db"];

/// 历史上由“迁移前备份/损坏隔离”逻辑产生的文件前缀。
const LEGACY_ARTIFACT_PREFIXES: &[&str] = &["lkapi.db.pre-upgrade-", "lkapi.db.broken-"];

pub struct Database {
    pub pool: SqlitePool,
}

impl Database {
    pub async fn new(app: &AppHandle) -> Self {
        let app_data_dir = app
            .path()
            .app_data_dir()
            .expect("failed to get app data dir");
        Self::new_with_path(&app_data_dir).await
    }

    /// headless（lkapi-web）入口：显式指定数据目录。
    pub async fn new_with_path(app_data_dir: &Path) -> Self {
        std::fs::create_dir_all(app_data_dir).expect("failed to create app data dir");

        let db_path = app_data_dir.join(DB_FILE_NAME);

        purge_legacy_files(app_data_dir, &db_path);

        let pool = Self::connect_and_migrate(&db_path)
            .await
            .expect("failed to prepare database");

        // 补建升级前切片的关键词投影，不改动已有正文和向量。
        crate::services::knowledge::repository::KbRepository::new(pool.clone())
            .backfill_search_text()
            .await
            .expect("failed to backfill knowledge search index");

        // Seed built-in security rules if table exists and is empty
        let _ = crate::security::rules::seed_builtin_rules(&pool).await;

        Self { pool }
    }

    /// 连接并跑迁移。**任何失败都视为“旧库不可用”**，直接丢弃整个数据库文件重来。
    ///
    /// 本应用不再尝试复用/修补历史版本的 schema：迁移 checksum 不匹配、schema
    /// 缺列、文件损坏，一律删库重建，避免把兼容代码和迁移补丁留在启动路径上。
    async fn connect_and_migrate(db_path: &Path) -> Result<SqlitePool, String> {
        match open_and_migrate(db_path).await {
            Ok(pool) => Ok(pool),
            Err(first_error) => {
                let removed = discard_database(db_path);
                log::warn!(
                    "数据库无法按当前 schema 打开（{first_error}），已删除重建: {}",
                    removed.join(", ")
                );
                open_and_migrate(db_path)
                    .await
                    .map_err(|second_error| {
                        format!("重建数据库失败: {second_error}（首次失败: {first_error}）")
                    })
            }
        }
    }
}

/// 打开数据库并跑全部迁移，任何一步失败都返回错误。
async fn open_and_migrate(db_path: &Path) -> Result<SqlitePool, String> {
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .map_err(|e| format!("连接数据库失败: {e}"))?;

    if let Err(e) = sqlx::migrate!("./migrations").run(&pool).await {
        pool.close().await;
        return Err(format!("执行数据库迁移失败: {e}"));
    }

    Ok(pool)
}

/// 删除数据库文件及其 SQLite 伴生文件。返回被删除文件的路径（用于日志）。
fn discard_database(db_path: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    for path in sqlite_files(db_path) {
        if std::fs::remove_file(&path).is_ok() {
            removed.push(path.display().to_string());
        }
    }
    removed
}

/// 主库文件 + SQLite 运行期伴生文件（`-wal` / `-shm` / `-journal`）。
fn sqlite_files(db_path: &Path) -> Vec<PathBuf> {
    let mut files = vec![db_path.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        let name = format!("{}{suffix}", DB_FILE_NAME);
        files.push(db_path.with_file_name(name));
    }
    files
}

/// 清理历史版本在同一目录里留下的数据库与备份残留。
///
/// 只动“确定属于旧数据”的文件：旧产品名的库、旧迁移备份、旧损坏隔离文件。
/// 当前使用的 `lkapi.db` 不在这里处理（它由迁移失败路径负责重建）。
fn purge_legacy_files(app_data_dir: &Path, db_path: &Path) {
    let Ok(entries) = std::fs::read_dir(app_data_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path == db_path {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };

        let is_legacy_db = LEGACY_DB_STEMS.iter().any(|stem| {
            name == *stem
                || name.starts_with(&format!("{stem}-"))
                || name.starts_with(&format!("{stem}."))
        });
        let is_legacy_artifact = LEGACY_ARTIFACT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix));

        if is_legacy_db || is_legacy_artifact {
            if std::fs::remove_file(&path).is_ok() {
                log::info!("已清理旧版本数据残留: {name}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lkapi-db-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sqlite_files_covers_wal_and_journal_companions() {
        let files = sqlite_files(Path::new("/tmp/lkapi/lkapi.db"));
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 4, "应覆盖主库与 3 个伴生文件: {names:?}");
        assert!(names.contains(&"lkapi.db".to_string()));
        assert!(names.contains(&"lkapi.db-wal".to_string()));
        assert!(names.contains(&"lkapi.db-shm".to_string()));
        assert!(names.contains(&"lkapi.db-journal".to_string()));
    }

    #[test]
    fn purge_removes_legacy_files_and_keeps_current_db() {
        let dir = temp_dir("purge");
        let db_path = dir.join(DB_FILE_NAME);

        for name in [
            "lkapi.db",
            "lkapi.db-wal",
            "lkapi.db.pre-upgrade-20260101-000000",
            "lkapi.db.broken-20260101-000000",
            "waliapi.db",
            "waliapi.db.pre-upgrade-20260101-000000",
            "xapi.db",
            "settings.json",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        purge_legacy_files(&dir, &db_path);

        assert!(dir.join("lkapi.db").exists(), "当前库文件不应被清理");
        assert!(
            dir.join("lkapi.db-wal").exists(),
            "当前库的伴生文件不应被清理"
        );
        assert!(dir.join("settings.json").exists(), "配置文件不应被清理");
        for name in [
            "lkapi.db.pre-upgrade-20260101-000000",
            "lkapi.db.broken-20260101-000000",
            "waliapi.db",
            "waliapi.db.pre-upgrade-20260101-000000",
            "xapi.db",
        ] {
            assert!(!dir.join(name).exists(), "旧数据应被清理: {name}");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn fresh_directory_gets_a_full_schema() {
        let dir = temp_dir("fresh");
        let db_path = dir.join(DB_FILE_NAME);

        let pool = Database::connect_and_migrate(&db_path)
            .await
            .expect("全新目录应能建库");

        let expected: i64 = sqlx::migrate!("./migrations")
            .iter()
            .map(|m| m.version)
            .max()
            .unwrap_or(0);
        let applied: i64 =
            sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(applied, expected, "应跑到最新迁移");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 旧版本留下的库（迁移 checksum 与当前文件不一致）不再试图修补，
    /// 而是整体丢弃重建：这是“不要管老版本用户数据”的落地点。
    #[tokio::test]
    async fn incompatible_legacy_database_is_replaced_instead_of_repaired() {
        let dir = temp_dir("legacy");
        let db_path = dir.join(DB_FILE_NAME);

        // 用一个 SQLite 库模拟旧版 schema：只有一个 `_sqlx_migrations` 表，
        // 版本号与 checksum 都与当前迁移集对不上。
        {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
                .await
                .unwrap();
            sqlx::query("CREATE TABLE _sqlx_migrations (version INTEGER PRIMARY KEY, description TEXT NOT NULL, installed_on TEXT NOT NULL, success INTEGER NOT NULL, checksum BLOB NOT NULL, execution_time INTEGER NOT NULL)")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (1, 'legacy', '2026-01-01T00:00:00Z', 1, X'00', 0)")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        let pool = Database::connect_and_migrate(&db_path)
            .await
            .expect("旧库应被丢弃后重建成功");

        let expected: i64 = sqlx::migrate!("./migrations")
            .iter()
            .map(|m| m.version)
            .max()
            .unwrap_or(0);
        let applied: i64 =
            sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(applied, expected, "重建后应跑到最新迁移");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
