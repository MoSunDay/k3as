//! Hot-reloadable route configuration (T5.4 Scope B / T5.5 seam).
//!
//! The live route table is held in a [`RouteStore`] — an
//! [`Rc`]`<`[`RefCell`]`<`[`Rc`]`<`[`RouteTable`]`>>>` so the single-threaded VM
//! (ADR **Q12**) can swap the whole table between requests with a cheap
//! `Rc` clone (no deep copy, no lock, no `AtomicUsize`). Each in-flight
//! request keeps its own `Rc<RouteTable>` snapshot taken at accept time, so a
//! swap never disturbs a request already being served — the "generation swap on
//! the request boundary" model. Full drain semantics (in-flight requests
//! finishing on the old route) are T5.5; M1 only needs the between-request swap.
//!
//! A [`ConfigSource`] yields successive compiled tables; a minimal
//! in-process channel watch is the M1 stub for the etcd/informer source
//! (allowed by Q5 / R4). The real `kube-rs` informer lands in T5.5/Phase 2.

use std::cell::RefCell;
use std::rc::Rc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::route::RouteTable;

/// The live, swappable route table.
///
/// Cloning a [`RouteStore`] shares the underlying table (cheap `Rc`); use it to
/// hand a handle to the data plane while another owner drives reloads.
#[derive(Clone)]
pub struct RouteStore {
    table: Rc<RefCell<Rc<RouteTable>>>,
}

impl RouteStore {
    /// Install an initial table (generation 0).
    pub fn new(table: RouteTable) -> Self {
        Self { table: Rc::new(RefCell::new(Rc::new(table))) }
    }

    /// Take a cheap snapshot of the *current* table (`Rc` clone — no deep copy).
    /// Safe to hold across an `await`: the returned `Rc` is independent of any
    /// later [`Self::install`].
    pub fn snapshot(&self) -> Rc<RouteTable> {
        self.table.borrow().clone()
    }

    /// Atomically swap in a new table, stamping the next generation. Returns
    /// the new generation. Must be called between requests (never while a
    /// [`Self::snapshot`] `RefCell` borrow is live — snapshots clone the `Rc`,
    /// so they never hold the borrow across this call).
    pub fn install(&self, mut table: RouteTable) -> u64 {
        let next = self.generation().wrapping_add(1);
        table.generation = next;
        *self.table.borrow_mut() = Rc::new(table);
        next
    }

    /// The generation of the currently-installed table.
    pub fn generation(&self) -> u64 {
        self.table.borrow().generation
    }
}

/// A source of successive compiled [`RouteTable`]s — the minimal seam the real
/// `kube-rs` informer (T5.5) will satisfy.
pub trait ConfigSource {
    /// Yield the next route table, or `None` when the source is exhausted.
    fn next_table(&mut self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RouteTable>>>>;
}

/// A static one-shot source: yields its table once, then `None` forever.
pub struct StaticConfigSource {
    table: Option<RouteTable>,
}

impl StaticConfigSource {
    pub fn new(table: RouteTable) -> Self {
        Self { table: Some(table) }
    }
}

impl ConfigSource for StaticConfigSource {
    fn next_table(&mut self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RouteTable>>>> {
        let table = self.table.take();
        Box::pin(async move { table })
    }
}

/// Create a channel pair for in-process hot reload: senders push new tables,
/// the proxy's `serve_proxy` polls the receiver and swaps via [`RouteStore`].
pub fn reload_channel() -> (UnboundedSender<RouteTable>, UnboundedReceiver<RouteTable>) {
    tokio::sync::mpsc::unbounded_channel()
}
