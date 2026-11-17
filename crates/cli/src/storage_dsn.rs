//! `--datastore-endpoint` selection (**T2.3**, decisions **Q29/Q33**),
//! split out of `runtime.rs` (file-size limit): the pure DSN classifier
//! plus the backend opener the server path drives.

use std::path::PathBuf;
use std::sync::Arc;

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
}
