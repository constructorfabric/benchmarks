use figment::Figment;
use figment::providers::Serialized;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::TempDir;
use toolkit_db::{DbConnConfig, DbManager, GlobalDatabaseConfig, PoolCfg};

#[tokio::test]
async fn test_dbmanager_sqlite_with_file() {
    let temp_dir = TempDir::new().unwrap();
    let db_filename = format!("test_manager_{}.db", std::process::id());

    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": db_filename,
                    "params": {
                        "journal_mode": "WAL"
                    }
                }
            }
        }
    })));

    let home_dir = temp_dir.path().to_path_buf();

    let manager = DbManager::from_figment(figment, home_dir).unwrap();

    // Should successfully create SQLite database
    let result = manager.get("test_gear").await.unwrap();
    assert!(result.is_some());

    let db = result.unwrap();
    assert_eq!(db.db_engine(), "sqlite");

    let expected_path = temp_dir.path().join("test_gear").join(&db_filename);
    assert!(
        expected_path.exists(),
        "Expected SQLite file at {}",
        expected_path.display()
    );
}

#[tokio::test]
async fn test_dbmanager_sqlite_with_path() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("absolute.db");

    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "path": db_path,
                    "params": {
                        "journal_mode": "DELETE"
                    }
                }
            }
        }
    })));

    let home_dir = temp_dir.path().to_path_buf();

    let manager = DbManager::from_figment(figment, home_dir).unwrap();

    // Should successfully create SQLite database at absolute path
    let result = manager.get("test_gear").await.unwrap();
    assert!(result.is_some());

    let db = result.unwrap();
    assert_eq!(db.db_engine(), "sqlite");
    assert!(
        db_path.exists(),
        "Expected SQLite file at {}",
        db_path.display()
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_dbmanager_caching() {
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "dsn": "sqlite::memory:",
                    "params": {
                        "journal_mode": "WAL"
                    }
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let home_dir = temp_dir.path().to_path_buf();

    let manager = DbManager::from_figment(figment, home_dir).unwrap();

    // First call should create the db
    let result1 = manager.get("test_gear").await.unwrap();
    assert!(result1.is_some());

    // Second call should return cached db (sharing is an internal detail)
    let result2 = manager.get("test_gear").await.unwrap();
    assert!(result2.is_some());

    let db1 = result1.unwrap();
    let db2 = result2.unwrap();
    assert_eq!(db1.db_engine(), "sqlite");
    assert_eq!(db2.db_engine(), "sqlite");
}

#[tokio::test]
async fn test_dbmanager_sqlite_server_without_dsn() {
    // Test that SQLite servers without DSN work correctly with gear file specification
    let global_config = GlobalDatabaseConfig {
        servers: {
            let mut servers = HashMap::new();
            servers.insert(
                "sqlite_server".to_owned(),
                DbConnConfig {
                    engine: Some(toolkit_db::config::DbEngineCfg::Sqlite),
                    params: Some({
                        let mut params = HashMap::new();
                        params.insert("WAL".to_owned(), "true".to_owned());
                        params.insert("synchronous".to_owned(), "NORMAL".to_owned());
                        params
                    }),
                    pool: Some(PoolCfg {
                        max_conns: Some(10),
                        acquire_timeout: Some(Duration::from_secs(30)),
                        ..Default::default()
                    }),
                    ..Default::default() // No DSN - gear specifies file
                },
            );
            servers
        },
        auto_provision: Some(true),
    };

    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "database": global_config,
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "server": "sqlite_server",
                    "file": format!("gear_{}.db", std::process::id())  // Should be placed in gear home directory
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let home_dir = temp_dir.path().to_path_buf();

    let manager = DbManager::from_figment(figment, home_dir.clone()).unwrap();

    // Should successfully create SQLite database in gear subdirectory
    let result = manager.get("test_gear").await.unwrap();
    assert!(result.is_some());

    let db = result.unwrap();
    assert_eq!(db.db_engine(), "sqlite");

    // Verify the database was created in the correct location (the filename will be dynamically generated)
    let gear_dir = home_dir.join("test_gear");
    assert!(
        gear_dir.exists(),
        "Gear directory should be created at {gear_dir:?}"
    );
    // Check if any .db file exists in the gear directory
    let db_files: Vec<_> = std::fs::read_dir(&gear_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "db" {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    assert!(
        !db_files.is_empty(),
        "At least one .db file should be created in {gear_dir:?}"
    );
}
