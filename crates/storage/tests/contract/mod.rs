//! Shared cross-backend storage contract (T2.3, Q29).
//!
//! The 26 backend-portable semantics cases that used to live embedded-only
//! in `tests/embedded_storage.rs`, expressed as generic functions over any
//! [`StorageBackend`] so both backends run the exact same assertions:
//!
//! - `tests/embedded_storage.rs` instantiates them for `EmbeddedStorage`
//!   (module `embedded`, reported as `embedded::<case>`);
//! - `tests/sqlite_storage.rs` instantiates them for `SqliteStorage` on a
//!   fresh `:memory:` database per case (module `sqlite_memory`).
//!
//! The single embedded-only case (`history_eviction_...` pins
//! `with_history_capacity`, which the trait does not expose) stays in
//! `tests/embedded_storage.rs`. Net effect of the split: 26 embedded cases
//! became 25 shared + 1 embedded-only, and the suite doubled across the
//! two backends (Q29: identical etcd-faithful semantics, different
//! durability substrates). A 26th shared case (deep-review hardening
//! #4: watch at an unrepresentable start revision) joined post-review.
//!
//! Cases take `Arc<S>` (not `S`) because one case spawns a background
//! writer that must share the store handle across the `tokio::spawn`
//! boundary; both instantiations construct their store the same way.
//!
//! Layout: helpers + the `storage_contract!` macro live here; the case
//! bodies are split by responsibility into `cases_crud.rs` (CRUD + list +
//! key layout), `cases_watch.rs` (live watch + replay), and
//! `cases_compact.rs` (explicit compaction) to stay under the file-size
//! budget.

pub(crate) mod cases_compact;
pub(crate) mod cases_crud;
pub(crate) mod cases_watch;

use std::time::Duration;

use storage::{Key, Watch, WatchEvent};
use tokio::time::timeout;

/// Instantiate the 26 shared contract cases for one backend.
///
/// ```ignore
/// storage_contract!(embedded, || async { Arc::new(EmbeddedStorage::new()) });
/// ```
///
/// The factory is an async closure returning `Arc<S>`; every generated
/// test builds a fresh store, so cases never share state. The suffix names
/// a nested module -- the paste-free equivalent of `<case>_<suffix>` test
/// names -- and the generated wrappers reach the case bodies through the
/// invoking target's root `mod contract;`.
macro_rules! storage_contract {
    ($suffix:ident, $factory:expr) => {
        mod $suffix {
            use super::*;

            // --- CRUD (cases_crud.rs) ---

            #[tokio::test]
            async fn create_then_get_round_trips() {
                let store = ($factory)().await;
                contract::cases_crud::create_then_get_round_trips(store).await;
            }

            #[tokio::test]
            async fn create_conflict_on_duplicate() {
                let store = ($factory)().await;
                contract::cases_crud::create_conflict_on_duplicate(store).await;
            }

            #[tokio::test]
            async fn get_missing_returns_none() {
                let store = ($factory)().await;
                contract::cases_crud::get_missing_returns_none(store).await;
            }

            #[tokio::test]
            async fn update_bumps_revision_and_version() {
                let store = ($factory)().await;
                contract::cases_crud::update_bumps_revision_and_version(store).await;
            }

            #[tokio::test]
            async fn update_with_stale_revision_conflicts() {
                let store = ($factory)().await;
                contract::cases_crud::update_with_stale_revision_conflicts(store).await;
            }

            #[tokio::test]
            async fn update_missing_not_found() {
                let store = ($factory)().await;
                contract::cases_crud::update_missing_not_found(store).await;
            }

            #[tokio::test]
            async fn delete_removes_and_returns_entry() {
                let store = ($factory)().await;
                contract::cases_crud::delete_removes_and_returns_entry(store).await;
            }

            #[tokio::test]
            async fn delete_with_stale_revision_conflicts() {
                let store = ($factory)().await;
                contract::cases_crud::delete_with_stale_revision_conflicts(store).await;
            }

            #[tokio::test]
            async fn list_filters_by_prefix_and_namespace() {
                let store = ($factory)().await;
                contract::cases_crud::list_filters_by_prefix_and_namespace(store).await;
            }

            #[tokio::test]
            async fn list_revision_ordered() {
                let store = ($factory)().await;
                contract::cases_crud::list_revision_ordered(store).await;
            }

            #[tokio::test]
            async fn list_does_not_match_partial_resource_segment() {
                let store = ($factory)().await;
                contract::cases_crud::list_does_not_match_partial_resource_segment(store).await;
            }

            #[tokio::test]
            async fn key_layout_matches_upstream_registry() {
                let store = ($factory)().await;
                contract::cases_crud::key_layout_matches_upstream_registry(store).await;
            }

            // --- Watch: live + replay (cases_watch.rs) ---

            #[tokio::test]
            async fn watch_delivers_put_and_delete_events() {
                let store = ($factory)().await;
                contract::cases_watch::watch_delivers_put_and_delete_events(store).await;
            }

            #[tokio::test]
            async fn watch_filters_by_prefix() {
                let store = ($factory)().await;
                contract::cases_watch::watch_filters_by_prefix(store).await;
            }

            #[tokio::test]
            async fn current_revision_starts_zero_and_monotonic() {
                let store = ($factory)().await;
                contract::cases_watch::current_revision_starts_zero_and_monotonic(store).await;
            }

            #[tokio::test]
            async fn watch_with_start_revision_replays_history_in_order() {
                let store = ($factory)().await;
                contract::cases_watch::watch_with_start_revision_replays_history_in_order(store)
                    .await;
            }

            #[tokio::test]
            async fn watch_replay_seam_is_lossless_and_duplicate_free() {
                let store = ($factory)().await;
                contract::cases_watch::watch_replay_seam_is_lossless_and_duplicate_free(store)
                    .await;
            }

            #[tokio::test]
            async fn watch_replay_filters_by_prefix() {
                let store = ($factory)().await;
                contract::cases_watch::watch_replay_filters_by_prefix(store).await;
            }

            #[tokio::test]
            async fn watch_from_future_revision_skips_older_events() {
                let store = ($factory)().await;
                contract::cases_watch::watch_from_future_revision_skips_older_events(store).await;
            }

            #[tokio::test]
            async fn watch_at_unrepresentable_revision_replays_nothing() {
                let store = ($factory)().await;
                contract::cases_watch::watch_at_unrepresentable_revision_replays_nothing(store)
                    .await;
            }

            #[tokio::test]
            async fn watch_without_start_revision_is_live_only() {
                let store = ($factory)().await;
                contract::cases_watch::watch_without_start_revision_is_live_only(store).await;
            }

            #[tokio::test]
            async fn delete_events_replay_final_object_and_deletion_revision() {
                let store = ($factory)().await;
                contract::cases_watch::delete_events_replay_final_object_and_deletion_revision(
                    store,
                )
                .await;
            }

            #[tokio::test]
            async fn two_watchers_replay_independently() {
                let store = ($factory)().await;
                contract::cases_watch::two_watchers_replay_independently(store).await;
            }

            // --- Explicit compaction (cases_compact.rs) ---

            #[tokio::test]
            async fn explicit_compact_returns_watermark_and_gates_watch() {
                let store = ($factory)().await;
                contract::cases_compact::explicit_compact_returns_watermark_and_gates_watch(store)
                    .await;
            }

            #[tokio::test]
            async fn compact_keeps_get_and_list_intact() {
                let store = ($factory)().await;
                contract::cases_compact::compact_keeps_get_and_list_intact(store).await;
            }

            #[tokio::test]
            async fn compact_clamps_future_revision_to_current() {
                let store = ($factory)().await;
                contract::cases_compact::compact_clamps_future_revision_to_current(store).await;
            }
        }
    };
}

pub(crate) use storage_contract;

/// Contract-test pod key: `/registry/pods/<ns>/<name>`.
pub(crate) fn pod_key(ns: &str, name: &str) -> Key {
    Key::new("", "pods", ns, name)
}

/// A minimal Pod-shaped JSON object.
pub(crate) fn pod_value(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": "default" },
        "spec": { "replicas": 1 }
    })
}

/// Recv the next event or panic; `None` (closed stream) is a failure.
pub(crate) async fn must_recv(w: &mut Watch) -> WatchEvent {
    match timeout(Duration::from_secs(2), w.recv()).await {
        Ok(Some(ev)) => ev,
        Ok(None) => panic!("watch stream closed unexpectedly"),
        Err(_) => panic!("timed out waiting for a watch event"),
    }
}

/// Assert no further event arrives within a short window.
pub(crate) async fn assert_no_event(w: &mut Watch) {
    assert!(
        timeout(Duration::from_millis(150), w.recv()).await.is_err(),
        "unexpected extra event delivered"
    );
}
