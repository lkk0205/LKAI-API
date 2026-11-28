pub mod models;
pub mod repository;

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tauri::{AppHandle, Manager};

/// 数据库文件名。
const DB_FILE_NAME: &str = "lkapi.db";

/// 迁移前备份文件名前缀。备份形如 `lkapi.db.pre-upgrade-20260806-190400`，与数据库同目录。
const BACKUP_PREFIX: &str = "lkapi.db.pre-upgrade-";

/// 保留的最近备份份数（超出后删除最旧的）。
const BACKUP_KEEP: usize = 3;

/// 无法按当前 schema 打开时，原库被隔离重命名用的前缀。
const QUARANTINE_PREFIX: &str = "lkapi.db.broken-";

/// 旧产品名留下的数据库文件名（`waliapi.db` 等）。
///
/// 这些不是本产品的数据（不同 app data 目录），启动时清掉遗留文件。
const LEGACY_DB_STEMS: &[&str] = &["waliapi.db", "xapi.db"];

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

    /// 连接并跑迁移。**默认保留用户数据**，只有库本身打不开或迁移彻底失败时才隔离重建。
    ///
    /// 启动顺序：连接 → 迁移前文件级备份 → 行尾 checksum 容错修复 → 跑迁移。
    /// 迁移失败时把原库（含 -wal/-shm 伴生文件）整体重命名为 `lkapi.db.broken-<时间戳>`
    /// 后新建空库启动，绝不静默删除用户数据。
    async fn connect_and_migrate(db_path: &Path) -> Result<SqlitePool, String> {
        let pool = match open_pool(db_path).await {
            Ok(pool) => pool,
            Err(first_error) => {
                let moved = quarantine_database(db_path);
                log::error!(
                    "数据库无法打开（{first_error}），原库已隔离: {}",
                    moved.join(", ")
                );
                open_pool(db_path)
                    .await
                    .map_err(|second_error| {
                        format!("重建数据库失败: {second_error}（首次失败: {first_error}）")
                    })?
            }
        };

        // 迁移前自动备份：schema 落后时先做文件级快照，再跑迁移。
        // 备份失败只记录，不阻断启动。
        if let Err(error) = backup_before_migration(&pool, db_path).await {
            log::error!("迁移前自动备份失败，继续启动: {error}");
        }

        // 兼容旧 Windows 安装包内嵌的 CRLF 迁移校验值（必须在备份之后）。
        match fix_migration_line_ending_checksums(&pool).await {
            Ok(repaired) if repaired > 0 => {
                log::warn!("已兼容 {repaired} 条历史迁移的 LF/CRLF 校验值差异");
            }
            Ok(_) => {}
            Err(error) => log::error!("迁移校验值修复失败，继续尝试迁移: {error}"),
        }

        match sqlx::migrate!("./migrations").run(&pool).await {
            Ok(()) => Ok(pool),
            Err(error) => {
                pool.close().await;
                let moved = quarantine_database(db_path);
                log::error!(
                    "执行数据库迁移失败（{error}），原库已隔离: {}",
                    moved.join(", ")
                );

                let fresh = open_pool(db_path).await.map_err(|second_error| {
                    format!("重建数据库失败: {second_error}（迁移失败: {error}）")
                })?;
                sqlx::migrate!("./migrations")
                    .run(&fresh)
                    .await
                    .map_err(|second_error| {
                        format!("重建数据库迁移失败: {second_error}（首次失败: {error}）")
                    })?;
                log::warn!("已使用新库启动；旧数据保留在隔离文件中，可手工恢复");
                Ok(fresh)
            }
        }
    }
}

/// 打开数据库连接池（不存在则创建文件）。
async fn open_pool(db_path: &Path) -> Result<SqlitePool, String> {
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());

    SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .map_err(|e| format!("连接数据库失败: {e}"))
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

/// 把打不开/迁移失败的库整体隔离：主库与伴生文件一起改名，保证可原样恢复。
/// 返回被改名的路径（用于日志）。
fn quarantine_database(db_path: &Path) -> Vec<String> {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let base_name = format!("{QUARANTINE_PREFIX}{ts}");
    let mut moved = Vec::new();

    for path in sqlite_files(db_path) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let suffix = name.strip_prefix(DB_FILE_NAME).unwrap_or("");
        let target = path.with_file_name(format!("{base_name}{suffix}"));
        if std::fs::rename(&path, &target).is_ok() {
            moved.push(target.display().to_string());
        }
    }

    moved
}

/// 清理历史版本在同一目录里留下的旧产品名数据库。
///
/// 迁移前备份（`lkapi.db.pre-upgrade-*`）与隔离文件（`lkapi.db.broken-*`）
/// **不在这里清理**：它们是用户数据的回滚点。
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

        if is_legacy_db {
            if std::fs::remove_file(&path).is_ok() {
                log::info!("已清理旧产品名数据库残留: {name}");
            }
        }
    }
}

/// 迁移集内的最大版本号（当前编译进二进制的迁移文件）。
fn migration_max_version() -> i64 {
    sqlx::migrate!("./migrations")
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// 数据库当前迁移版本：`_sqlx_migrations` 中最大的已成功版本。
/// 表不存在或为空时返回 0（全新数据库）。
async fn current_db_version(pool: &SqlitePool) -> i64 {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(pool)
            .await
            .ok()
            .flatten();
    max.unwrap_or(0)
}

/// 生成本次备份路径：`lkapi.db.pre-upgrade-<YYYYmmdd-HHMMSS>`，与数据库同目录。
fn make_backup_path(db_path: &Path) -> PathBuf {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    db_path.with_file_name(format!("{BACKUP_PREFIX}{ts}"))
}

/// 删除同目录下过旧的迁移前备份，只保留最近 `BACKUP_KEEP` 份。
fn prune_old_backups(db_path: &Path) -> Result<(), String> {
    let dir = db_path.parent().ok_or("数据库路径缺少父目录")?;
    let mut backups: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| format!("读取备份目录失败: {e}"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().to_string();
            if !name.starts_with(BACKUP_PREFIX) {
                return None;
            }
            let mtime = std::fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((mtime, path))
        })
        .collect();

    backups.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_mtime, path) in backups.into_iter().skip(BACKUP_KEEP) {
        std::fs::remove_file(&path)
            .map_err(|e| format!("删除旧备份 {} 失败: {e}", path.display()))?;
    }
    Ok(())
}

/// 迁移前自动备份。仅当数据库已存在（有迁移记录）且 schema 版本低于当前迁移集时执行。
///
/// 备份用 SQLite `VACUUM INTO` 生成一致事务快照（包含已提交数据），命名
/// `lkapi.db.pre-upgrade-<YYYYmmdd-HHMMSS>`，随后按 `BACKUP_KEEP` 清理旧备份。
///
/// 无需备份时返回 `Ok(None)`。备份失败由调用方记录，不阻断启动。
async fn backup_before_migration(
    pool: &SqlitePool,
    db_path: &Path,
) -> Result<Option<PathBuf>, String> {
    let db_max = current_db_version(pool).await;
    let migration_max = migration_max_version();

    if db_max == 0 || db_max >= migration_max {
        return Ok(None);
    }

    let backup_path = make_backup_path(db_path);
    // VACUUM INTO 拒绝非空目标，先清除旧目标（同名同秒重复时防御）
    if backup_path.exists() {
        std::fs::remove_file(&backup_path)
            .map_err(|e| format!("移除旧备份 {} 失败: {e}", backup_path.display()))?;
    }
    let dest = backup_path.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{dest}'"))
        .execute(pool)
        .await
        .map_err(|e| format!("创建备份失败: {e}"))?;

    log::info!(
        "迁移前已备份数据库 (schema {db_max} -> {migration_max}): {}",
        backup_path.display()
    );

    prune_old_backups(db_path)?;

    Ok(Some(backup_path))
}

/// 兼容旧 Windows 安装包内嵌的 CRLF 迁移，保留未知 SQL 内容差异的校验。
///
/// 只接受「当前 SQL 的 LF / CRLF 两种行尾变体」：历史 checksum 必须等于其中之一
/// 才会被改写成当前值，其他内容差异仍然拦下来，避免掩盖真正的迁移改动。
async fn fix_migration_line_ending_checksums(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    use sha2::Digest;

    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    let applied: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(&mut *tx)
            .await?;
    let migrator = sqlx::migrate!("./migrations");
    let mut repaired = 0u64;
    for (version, stored_checksum) in applied {
        let Some(migration) = migrator
            .iter()
            .find(|migration| migration.version == version)
        else {
            continue;
        };
        if migration.checksum.as_ref() == stored_checksum.as_slice() {
            continue;
        }

        let lf = migration.sql.replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        let lf_checksum = sha2::Sha384::digest(lf.as_bytes()).to_vec();
        let crlf_checksum = sha2::Sha384::digest(crlf.as_bytes()).to_vec();
        if stored_checksum != lf_checksum && stored_checksum != crlf_checksum {
            continue;
        }

        repaired += sqlx::query(
            "UPDATE _sqlx_migrations SET checksum = ? WHERE version = ? AND checksum = ? AND success = 1",
        )
        .bind(migration.checksum.as_ref())
        .bind(version)
        .bind(&stored_checksum)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;

    Ok(repaired)
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
    fn purge_removes_legacy_product_db_but_keeps_backups() {
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
        // 迁移前备份与隔离文件是用户数据的回滚点，必须保留。
        assert!(
            dir.join("lkapi.db.pre-upgrade-20260101-000000").exists(),
            "迁移前备份不应被清理"
        );
        assert!(
            dir.join("lkapi.db.broken-20260101-000000").exists(),
            "隔离文件不应被清理"
        );
        for name in [
            "waliapi.db",
            "waliapi.db.pre-upgrade-20260101-000000",
            "xapi.db",
        ] {
            assert!(!dir.join(name).exists(), "旧产品名数据应被清理: {name}");
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

        let applied: i64 =
            sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(applied, migration_max_version(), "应跑到最新迁移");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 迁移前备份：库版本落后于迁移集时，应生成 `lkapi.db.pre-upgrade-*` 快照。
    #[tokio::test]
    async fn backup_is_created_when_schema_is_behind() {
        let dir = temp_dir("backup");
        let db_path = dir.join(DB_FILE_NAME);

        let pool = Database::connect_and_migrate(&db_path)
            .await
            .expect("全新目录应能建库");
        // 把最后一条迁移记录抹掉，模拟“旧库落后一个版本”。
        let last = migration_max_version();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
            .bind(last)
            .execute(&pool)
            .await
            .unwrap();

        let backup = backup_before_migration(&pool, &db_path)
            .await
            .expect("备份流程不应报错")
            .expect("schema 落后时必须生成备份");
        assert!(backup.exists(), "备份文件应存在: {}", backup.display());
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(BACKUP_PREFIX),
            "备份命名应带前缀"
        );

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 旧库 checksum 与当前文件对不上：不再整库丢弃，而是隔离保留后重建。
    #[tokio::test]
    async fn incompatible_legacy_database_is_quarantined_not_deleted() {
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
            .expect("旧库应被隔离后重建成功");

        let applied: i64 =
            sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(applied, migration_max_version(), "重建后应跑到最新迁移");

        // 原库必须以 broken 文件形式保留下来
        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(QUARANTINE_PREFIX)
            });
        assert!(quarantined, "原库应被隔离保留，而不是删除");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// CRLF 行尾的旧校验值应被就地修复，业务数据不受影响。
    #[tokio::test]
    async fn crlf_migration_checksums_are_repaired_in_place() {
        use sha2::Digest;

        let dir = temp_dir("crlf");
        let db_path = dir.join(DB_FILE_NAME);

        let pool = Database::connect_and_migrate(&db_path)
            .await
            .expect("全新目录应能建库");

        // 模拟旧 Windows 安装包：把已应用迁移的 checksum 改写成 CRLF 变体。
        let migrator = sqlx::migrate!("./migrations");
        let mut tx = pool.begin().await.unwrap();
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

        let repaired = fix_migration_line_ending_checksums(&pool)
            .await
            .expect("行尾修复不应报错");
        assert!(repaired > 0, "CRLF 校验值必须被修复");

        // 修复后所有记录都应等于当前迁移文件的 checksum
        for migration in sqlx::migrate!("./migrations").iter() {
            let stored: Vec<u8> =
                sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
                    .bind(migration.version)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                stored.as_slice(),
                migration.checksum.as_ref(),
                "version {} 的 checksum 未修复",
                migration.version
            );
        }

        // 再跑一次迁移应通过（证明 0.4.0 老库可以直接升级）
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("修复后迁移应通过");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
