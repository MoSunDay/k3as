//! Re-export shim (T3.1b): the RFC3339 implementation moved to
//! [`common::time`] so the apiserver's finalizer-gated DELETE (deletion
//! `Timestamp` projection) and the controllers share one implementation.
//! Every pre-existing `crate::time::...` path keeps working through this
//! glob re-export; new code should depend on `common::time` directly.

pub use common::time::*;
