//! Id registry (T4.12, Q49): every sandbox/container the kubelet owns on
//! the builtin engine, mapped to its socket, process handles and state.
//! Plain data + a mutex; the pump tasks mutate states, the backend reads
//! snapshots for `list_*`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};

use super::pump;
use crate::cri_backend::{ContainerView, SandboxView};

/// A sandbox's pause engine child lives in the registry (no pump task);
/// a container's engine child lives in its pump task, so only the pid is
/// kept here for status/debug.
pub(crate) enum Proc {
    /// The pause engine child the backend reaps on stop/remove; taken
    /// once so a later remove sees `None` (already gone).
    Sandbox(Option<Child>),
    Container {
        engine_pid: u32,
    },
}

/// What the entry still remembers between create and start.
#[derive(Clone)]
pub(crate) struct Pending {
    pub ccfg: runtime::ContainerConfig,
    pub pcfg: runtime::PodSandboxConfig,
    pub sandbox_id: String,
}

pub(crate) struct Entry {
    pub id: String,
    pub socket: PathBuf,
    pub proc: Proc,
    pub state: String,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    /// `/proc/<pause_pid>/ns/net` for members to join (sandboxes only).
    pub netns: Option<String>,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub attempt: u32,
    pub labels: BTreeMap<String, String>,
    pub pending: Option<Pending>,
    /// Live control channel into the pump task (running containers).
    pub ctrl: Option<tokio::sync::mpsc::Sender<pump::Ctrl>>,
}

#[derive(Clone, Default)]
pub(crate) struct Registry {
    inner: Arc<Mutex<BTreeMap<String, Entry>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Poisoning only happens on a panicked pump; treat it as empty rather
    // than cascading panics into the sync loop.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Registry {
    pub fn insert(&self, entry: Entry) {
        lock(&self.inner).insert(entry.id.clone(), entry);
    }

    pub fn state(&self, id: &str) -> Option<(String, Option<i32>)> {
        lock(&self.inner)
            .get(id)
            .map(|e| (e.state.clone(), e.exit_code))
    }

    pub fn set_running(&self, id: &str, pid: u32) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            e.state = "CONTAINER_RUNNING".into();
            e.pid = Some(pid);
        }
    }

    pub fn set_exited(&self, id: &str, code: i32) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            e.state = "CONTAINER_EXITED".into();
            e.exit_code = Some(code);
        }
    }

    pub fn set_sandbox_notready(&self, id: &str) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            e.state = "SANDBOX_NOTREADY".into();
        }
    }

    pub fn remove(&self, id: &str) {
        lock(&self.inner).remove(id);
    }

    pub fn contains(&self, id: &str) -> bool {
        lock(&self.inner).contains_key(id)
    }

    /// One-shot take of the pause engine child (stop/remove reaps it).
    pub fn take_child(&self, id: &str) -> Option<Child> {
        let mut m = lock(&self.inner);
        match m.get_mut(id).map(|e| &mut e.proc) {
            Some(Proc::Sandbox(child)) => child.take(),
            _ => None,
        }
    }

    pub fn socket(&self, id: &str) -> Option<PathBuf> {
        lock(&self.inner).get(id).map(|e| e.socket.clone())
    }

    /// The create-time configs (kept in the entry: `container_views`
    /// reads `sandbox_id` from them).
    pub fn pending(&self, id: &str) -> Option<Pending> {
        lock(&self.inner).get(id).and_then(|e| e.pending.clone())
    }

    /// The sandbox netns members join — only while the sandbox is READY.
    pub fn netns_of(&self, sandbox_id: &str) -> Option<String> {
        lock(&self.inner)
            .get(sandbox_id)
            .filter(|e| e.state == "SANDBOX_READY")
            .and_then(|e| e.netns.clone())
    }

    pub fn set_ctrl(&self, id: &str, ctrl: tokio::sync::mpsc::Sender<pump::Ctrl>) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            e.ctrl = Some(ctrl);
        }
    }

    pub fn ctrl(&self, id: &str) -> Option<tokio::sync::mpsc::Sender<pump::Ctrl>> {
        lock(&self.inner).get(id).and_then(|e| e.ctrl.clone())
    }

    pub fn set_engine_pid(&self, id: &str, pid: u32) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            if let Proc::Container { engine_pid } = &mut e.proc {
                *engine_pid = pid;
            }
        }
    }

    /// Teardown reached without an Exited event (engine died): a
    /// running container is marked exited with the crash convention.
    pub fn set_exited_if_running(&self, id: &str) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            if e.state == "CONTAINER_RUNNING" {
                e.state = "CONTAINER_EXITED".into();
                e.exit_code.get_or_insert(255);
            }
        }
    }

    pub fn clear_ctrl(&self, id: &str) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            e.ctrl = None;
        }
    }

    /// CRI views for the sync loop (states mirror CRI strings).
    pub fn sandbox_views(&self) -> Vec<SandboxView> {
        lock(&self.inner)
            .values()
            .filter(|e| matches!(e.proc, Proc::Sandbox(_)))
            .map(|e| SandboxView {
                id: e.id.clone(),
                state: e.state.clone(),
                labels: e.labels.clone(),
                name: e.name.clone(),
                namespace: e.namespace.clone(),
                uid: e.uid.clone(),
                // Q52: builtin pods share the host netns — the sandbox
                // never gets CNI; ip stays None here and the backend
                // overlays the node IP on READY views.
                ip: None,
            })
            .collect()
    }

    pub fn container_views(&self) -> Vec<ContainerView> {
        lock(&self.inner)
            .values()
            .filter(|e| matches!(e.proc, Proc::Container { .. }))
            .map(|e| ContainerView {
                id: e.id.clone(),
                sandbox_id: e
                    .pending
                    .as_ref()
                    .map(|p| p.sandbox_id.clone())
                    .unwrap_or_default(),
                state: e.state.clone(),
                name: e.name.clone(),
                attempt: e.attempt,
                labels: e.labels.clone(),
                exit_code: e.exit_code.unwrap_or(0),
            })
            .collect()
    }
}
