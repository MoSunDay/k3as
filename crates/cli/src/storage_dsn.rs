//! `--datastore-endpoint` selection (**T2.3**, decisions **Q29/Q33**),
//! split out of `runtime.rs` (file-size limit): the pure DSN classifier
//! plus the backend opener the server path drives.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use infra::Shutdown;

/// `--datastore-endpoint` classification (**T2.3**, decision **Q29**): the
/// pure DSN-to-backend selector [`open_store`] switches on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dsn {
    /// Unset or empty: the zero-dependency embedded default (**Q17**).
    Embedded,
    /// `rdb://<path>`: the RocksDB (rdb `store` engine) backend
    /// (`RdbStorage`, **Q33**).
    Rdb(PathBuf),
    /// Anything else (including bare `sqlite://`): fatal, unsupported.
    Unsupported(String),
}

/// Classify a `--datastore-endpoint` DSN (**T2.3/Q29**). Pure: no I/O, no
/// side effects — the openable cases stay distinguishable for tests.
pub(crate) fn classify_dsn(dsn: Option<&str>) -> Dsn {
    match dsn {
        None | Some("") => Dsn::Embedded,
        Some(s) if s.starts_with("rdb://") => match &s["rdb://".len()..] {
            "" => Dsn::Unsupported(s.to_string()),
            path => Dsn::Rdb(PathBuf::from(path)),
        },
        Some(s) => Dsn::Unsupported(s.to_string()),
    }
}

/// Open the storage backend selected by `--datastore-endpoint` (**T2.3**,
/// decision **Q29**). Unset/empty keeps the embedded default (**Q17**) —
/// rdb is an explicit opt-in, never a silent behavior change;
/// `rdb://<path>` opens the RocksDB backend (**Q33**); other schemes are
/// fatal (etcd-gRPC is deferred to T3.4).
pub(crate) async fn open_store(
    dsn: Option<&str>,
) -> std::io::Result<Arc<dyn storage::StorageBackend>> {
    match classify_dsn(dsn) {
        Dsn::Embedded => Ok(Arc::new(storage::EmbeddedStorage::new())),
        Dsn::Rdb(path) => {
            let store = storage::RdbStorage::open(&path)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            tracing::info!(target: "init-pro", path = %path.display(),
                "rdb datastore (rocksdb) — T2.3/Q33");
            Ok(Arc::new(store))
        }
        Dsn::Unsupported(s) => {
            let msg = if s == "rdb://" {
                "--datastore-endpoint rdb:// requires a file path".to_string()
            } else {
                format!(
                    "unsupported --datastore-endpoint scheme (got {s:?}); \
                     supported: rdb://<path> (T2.3); empty = embedded default"
                )
            };
            Err(std::io::Error::other(msg))
        }
    }
}

/// B3 (long-run bug sweep): how often the durable history is compacted.
/// etcd keeps a 5-minute cadence; what bounds replay is the retain count
/// ([`storage::DEFAULT_RETAIN`]), not time.
const COMPACTION_INTERVAL: Duration = Duration::from_secs(300);

/// Pure gate (B3): only `rdb://` accumulates `log/<rev>` history on disk --
/// the embedded store already keeps a bounded in-memory ring.
pub(crate) fn compaction_enabled(dsn: &Dsn) -> bool {
    matches!(dsn, Dsn::Rdb(_))
}

/// Spawn the periodic history compaction (B3) for the selected DSN: `None`
/// when there is nothing to compact, a task handle the caller joins on
/// shutdown otherwise. The first pass runs one interval after boot.
pub(crate) fn spawn_compaction(
    dsn: &Dsn,
    store: Arc<dyn storage::StorageBackend>,
    shutdown: &Shutdown,
) -> Option<tokio::task::JoinHandle<()>> {
    compaction_enabled(dsn).then(|| {
        tokio::spawn(compact_periodically(
            store,
            shutdown.clone(),
            COMPACTION_INTERVAL,
            storage::DEFAULT_RETAIN,
        ))
    })
}

/// One [`storage::compact_retaining`] pass per `interval` until the token
/// fires. Compaction is hygiene: a failed pass is logged and retried at the
/// next tick, never fatal.
async fn compact_periodically(
    store: Arc<dyn storage::StorageBackend>,
    shutdown: Shutdown,
    interval: Duration,
    retain: u64,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(interval) => {
                match storage::compact_retaining(&*store, retain).await {
                    Ok(watermark) => {
                        tracing::debug!(target: "init-pro", watermark, "durable history compacted");
                    }
                    Err(e) => tracing::warn!(
                        target: "init-pro",
                        error = %e,
                        "durable history compaction failed; retrying next tick"
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T2.3/Q33: unset or empty DSN must keep the embedded default (Q17) -
    /// rdb is explicit opt-in only.
    #[test]
    fn classify_dsn_empty_is_embedded() {
        assert_eq!(classify_dsn(None), Dsn::Embedded);
        assert_eq!(classify_dsn(Some("")), Dsn::Embedded);
    }

    #[test]
    fn classify_dsn_rdb_paths() {
        assert_eq!(
            classify_dsn(Some("rdb:///tmp/a.rdb")),
            Dsn::Rdb(PathBuf::from("/tmp/a.rdb"))
        );
        assert_eq!(
            classify_dsn(Some("rdb://rel.rdb")),
            Dsn::Rdb(PathBuf::from("rel.rdb"))
        );
    }

    #[test]
    fn classify_dsn_rejects_bare_rdb_and_foreign_schemes() {
        assert_eq!(
            classify_dsn(Some("rdb://")),
            Dsn::Unsupported("rdb://".to_string())
        );
        assert_eq!(
            classify_dsn(Some("sqlite:///tmp/a.db")),
            Dsn::Unsupported("sqlite:///tmp/a.db".to_string())
        );
        assert_eq!(
            classify_dsn(Some("mysql://x")),
            Dsn::Unsupported("mysql://x".to_string())
        );
        assert_eq!(
            classify_dsn(Some("postgres://y")),
            Dsn::Unsupported("postgres://y".to_string())
        );
    }

    /// B3: the loop is rdb-only, and once running it bounds the on-disk
    /// history exactly like `compact_retaining` -- a watcher starting below
    /// the watermark is refused with `Compacted` (the 410 posture).
    #[tokio::test]
    async fn compaction_loop_is_rdb_only_and_bounds_history() {
        assert!(!compaction_enabled(&Dsn::Embedded));
        assert!(compaction_enabled(&Dsn::Rdb(PathBuf::from("/tmp/a.rdb"))));
        assert!(!compaction_enabled(&Dsn::Unsupported("x".into())));

        let shutdown = Shutdown::new();
        assert!(
            spawn_compaction(
                &Dsn::Embedded,
                Arc::new(storage::EmbeddedStorage::new()),
                &shutdown,
            )
            .is_none(),
            "the embedded store keeps a bounded ring: nothing to compact"
        );

        let dir = std::env::temp_dir().join(format!(
            "init-pro-cli-compaction-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let rdb = storage::RdbStorage::open(&dir).await.unwrap();
        let store: Arc<dyn storage::StorageBackend> = Arc::new(rdb);
        let prefix = storage::KeyPrefix::new("", "pods", Some("default".to_string()));
        for i in 1..=5u32 {
            store
                .create(
                    &storage::Key::new("", "pods", "default", format!("p{i}")),
                    serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": format!("p{i}")}}),
                )
                .await
                .unwrap();
        }
        assert_eq!(store.current_revision().await.unwrap(), 5);

        let loop_task = tokio::spawn(compact_periodically(
            store.clone(),
            shutdown.clone(),
            Duration::from_millis(20),
            2,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match store.watch(&prefix, Some(1)).await {
                Err(storage::StorageError::Compacted { .. }) => break,
                Ok(_) => {}
                Err(e) => panic!("unexpected watch error: {e}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "compaction loop never reached the watermark"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Revisions above the watermark still replay.
        assert!(store.watch(&prefix, Some(4)).await.is_ok());

        shutdown.trigger();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("loop must stop on shutdown")
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
