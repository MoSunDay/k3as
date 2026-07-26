//! `resty_stdlib` integration test entry point (T5.3 acceptance).
//!
//! The single-file test (`tests/resty_stdlib.rs`) was split — purely
//! mechanically — into per-library submodules to stay under the repo's 400-line
//! new-file cap. All test names, bodies, and assertions are unchanged; only the
//! helper call paths / imports were adjusted. Rust treats this `main.rs` as a
//! single integration-test binary still named `resty_stdlib`.

mod helpers;
mod lrucache;
mod shared_dict;
mod random_string;
mod http_lock;
