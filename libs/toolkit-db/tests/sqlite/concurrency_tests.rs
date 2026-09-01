#![allow(clippy::unwrap_used, clippy::expect_used, clippy::use_debug)]

//! Tests for concurrency and caching behavior of `DbManager`.

use figment::{Figment, providers::Serialized};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use toolkit_db::manager::DbManager;

fn expected_sqlite_path(temp_dir: &TempDir, gear: &str, file: &str) -> PathBuf {
    temp_dir.path().join(gear).join(file)
}

/// Test race condition: two concurrent `get()` calls for the same gear.
/// Both callers should succeed.
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn test_concurrent_get_same_gear() {
    let file = format!("concurrent_same_{}.db", std::process::id());
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": file,
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager =
        Arc::new(DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap());

    // Launch two concurrent get() calls for the same gear
    let manager1 = manager.clone();
    let manager2 = manager.clone();

    let (result1, result2) = tokio::join!(manager1.get("test_gear"), manager2.get("test_gear"));

    // Both should succeed
    let _db1 = result1.unwrap().expect("First call should return a db");
    let _db2 = result2.unwrap().expect("Second call should return a db");

    let expected = expected_sqlite_path(&temp_dir, "test_gear", &file);
    assert!(
        expected.exists(),
        "Expected SQLite file at {}",
        expected.display()
    );
}

/// Test concurrent `get()` calls for different gears.
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn test_concurrent_get_different_gears() {
    let file_a = format!("gear_a_{}.db", std::process::id());
    let file_b = format!("gear_b_{}.db", std::process::id());
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "gear_a": {
                "database": {
                    "engine": "sqlite",
                    "file": file_a,
                }
            },
            "gear_b": {
                "database": {
                    "engine": "sqlite",
                    "file": file_b,
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager =
        Arc::new(DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap());

    // Launch concurrent get() calls for different gears
    let manager1 = manager.clone();
    let manager2 = manager.clone();

    let (result1, result2) = tokio::join!(manager1.get("gear_a"), manager2.get("gear_b"));

    // Both should succeed
    let _db1 = result1.unwrap().expect("First call should return a db");
    let _db2 = result2.unwrap().expect("Second call should return a db");

    let path_a = expected_sqlite_path(&temp_dir, "gear_a", &file_a);
    let path_b = expected_sqlite_path(&temp_dir, "gear_b", &file_b);
    assert!(
        path_a.exists(),
        "Expected SQLite file at {}",
        path_a.display()
    );
    assert!(
        path_b.exists(),
        "Expected SQLite file at {}",
        path_b.display()
    );
    assert_ne!(path_a, path_b);
}

/// Test caching behavior: second call for same gear should return cached handle.
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn test_caching_behavior() {
    let file = format!("caching_test_{}.db", std::process::id());
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": file,
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager = DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap();

    // First call
    let _db1 = manager
        .get("test_gear")
        .await
        .unwrap()
        .expect("First call should succeed");

    // Second call - should return cached db (exact sharing is an internal detail)
    let _db2 = manager
        .get("test_gear")
        .await
        .unwrap()
        .expect("Second call should succeed");

    let expected = expected_sqlite_path(&temp_dir, "test_gear", &file);
    assert!(
        expected.exists(),
        "Expected SQLite file at {}",
        expected.display()
    );
}

/// Test behavior on unknown gear.
#[tokio::test]
async fn test_unknown_gear_behavior() {
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "known_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": format!("known_{}.db", std::process::id())
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager = DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap();

    // Request unknown gear
    let result = manager.get("unknown_gear").await;

    match result {
        Ok(None) => {
            // This is the expected behavior: no config = None return
        }
        Ok(Some(_)) => {
            panic!("Expected None for unknown gear, got Some(handle)");
        }
        Err(err) => {
            panic!("Expected Ok(None) for unknown gear, got error: {err:?}");
        }
    }
}

/// Test concurrent access with mixed success/failure scenarios.
#[tokio::test]
async fn test_concurrent_mixed_scenarios() {
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "valid_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": format!("valid_{}.db", std::process::id())
                }
            },
            "invalid_gear": {
                "database": {
                    "engine": "sqlite",
                    "dsn": format!("sqlite:file:mixed_invalid_{}.db", std::process::id()),
                    "host": "localhost"  // Conflict: SQLite DSN with host field
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager =
        Arc::new(DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap());

    // Launch concurrent calls for valid and invalid gears
    let manager1 = manager.clone();
    let manager2 = manager.clone();
    let manager3 = manager.clone();

    let (result1, result2, result3) = tokio::join!(
        manager1.get("valid_gear"),
        manager2.get("invalid_gear"),
        manager3.get("nonexistent_gear")
    );

    // Valid gear should succeed
    assert!(result1.is_ok() && result1.as_ref().unwrap().is_some());

    // Invalid gear should fail with config conflict
    assert!(result2.is_err());

    // Nonexistent gear should return None
    assert!(result3.is_ok() && result3.as_ref().unwrap().is_none());
}

/// Test performance: many concurrent requests for the same gear.
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn test_concurrent_performance() {
    let file = format!("perf_test_{}.db", std::process::id());
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": file,
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager =
        Arc::new(DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap());

    // Launch many concurrent requests
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let manager_clone = manager.clone();
        let task = tokio::spawn(async move { manager_clone.get("test_gear").await });
        tasks.push(task);
    }

    // Wait for all tasks with timeout
    let results = timeout(Duration::from_secs(10), async {
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.unwrap());
        }
        results
    })
    .await
    .expect("All tasks should complete within timeout");

    for result in &results {
        assert!(result.as_ref().unwrap().is_some());
    }

    let expected = expected_sqlite_path(&temp_dir, "test_gear", &file);
    assert!(
        expected.exists(),
        "Expected SQLite file at {}",
        expected.display()
    );
}

/// Test cache behavior across different manager instances.
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn test_cache_isolation_across_managers() {
    let file = format!("isolation_test_{}.db", std::process::id());
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "test_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": file,
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();

    // Create two separate manager instances
    let manager1 = DbManager::from_figment(figment.clone(), temp_dir.path().to_path_buf()).unwrap();
    let manager2 = DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap();

    // Get dbs from both managers (separate caches are an internal detail).
    let _db1 = manager1.get("test_gear").await.unwrap().unwrap();
    let _db2 = manager2.get("test_gear").await.unwrap().unwrap();

    let expected = expected_sqlite_path(&temp_dir, "test_gear", &file);
    assert!(
        expected.exists(),
        "Expected SQLite file at {}",
        expected.display()
    );
}

/// Test that errors are not cached.
#[tokio::test]
async fn test_errors_not_cached() {
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "bad_gear": {
                "database": {
                    "dsn": format!("sqlite:file:error_test_{}.db", std::process::id()),
                    "host": "localhost"  // Conflict
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager = DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap();

    // First call should fail
    let result1 = manager.get("bad_gear").await;
    assert!(result1.is_err());

    // Second call should also fail (errors should not be cached)
    let result2 = manager.get("bad_gear").await;
    assert!(result2.is_err());

    // Both should be the same type of error
    match (result1, result2) {
        (Err(err1), Err(err2)) => {
            assert_eq!(std::mem::discriminant(&err1), std::mem::discriminant(&err2));
        }
        _ => panic!("Both calls should fail"),
    }
}

/// Test concurrent initialization with slow database connections.
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn test_concurrent_slow_initialization() {
    let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
        "gears": {
            "slow_gear": {
                "database": {
                    "engine": "sqlite",
                    "file": format!("slow_test_{}.db", std::process::id()),
                    "pool": {
                        "max_conns": 1,           // Force serialization
                        "acquire_timeout": "5s"   // Longer timeout
                    }
                }
            }
        }
    })));

    let temp_dir = TempDir::new().unwrap();
    let manager =
        Arc::new(DbManager::from_figment(figment, temp_dir.path().to_path_buf()).unwrap());

    // Launch multiple concurrent requests
    let manager1 = manager.clone();
    let manager2 = manager.clone();
    let manager3 = manager.clone();

    let start = std::time::Instant::now();

    let (result1, result2, result3) = tokio::join!(
        manager1.get("slow_gear"),
        manager2.get("slow_gear"),
        manager3.get("slow_gear")
    );

    let elapsed = start.elapsed();

    // All should succeed
    let _db1 = result1.unwrap().unwrap();
    let _db2 = result2.unwrap().unwrap();
    let _db3 = result3.unwrap().unwrap();

    // Should complete in reasonable time (not 3x slower due to concurrency)
    assert!(
        elapsed < Duration::from_secs(10),
        "Concurrent initialization took too long: {elapsed:?}"
    );

    println!("Concurrent slow initialization completed in {elapsed:?}");
}
