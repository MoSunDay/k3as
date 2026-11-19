# Built-in container engine

Status: **done (T4.11-T4.14, Q48-Q55)** — P1 kernel + single-threaded
engine + protocol (Q48); P2 `kubelet::EngineBackend` + real
`--container-engine=builtin` selection with probe fallback (T4.12,
Q49-Q51); P3 registry-v2 pull chain + shared CAS layers + per-container
overlayfs views + image-config launch truth (T4.13, Q52-Q54); P4
host-rootfs default `--builtin-rootfs host|image` + host-mounts /
rootfs annotations (T4.14, Q55). The default runtime stays `containerd`
(Q51 go/no-go: builtin remains opt-in); CNI/flannel conformance face is
the open remainder (T4.3).

## What it is

`init-pro` can create containers itself: a `clone3` into fresh
namespaces, child-side mount/rootfs/pivot/cap/setuid setup, and an
`execve` — with no runc, no containerd, and no shim. The decision and
its invariants are Q48 (`plans/init-pro/decisions.md`); the kernel is a
Rust port of kiri's `kiri-init` Linux child model (Apache-2.0, pin
`7846fe0`). kiri's GPL-2.0 `guest/kernel` tree is never used.

```
kubelet (tokio)                  builtin engine process (single thread)
  │  engine-client                  │
  └─ Unix socket ── frames ────────►│  engine serve --socket
                                    │    ├─ clone3(child_stack=0)
                                    │    ├─ child: mounts + pivot_root
                                    │    │          caps/no_new_privs
                                    │    │          setgid/setuid/execve
                                    │    └─ parent: start gate + waitpid
                                    └─ cgroup v2 init-pro/<id>
```

## Crates

| Crate | Role |
|-------|------|
| `nsrun` | The only crate allowed to contain `unsafe`: clone3, namespaces, mount recipe, pivot_root, caps, cgroup v2, preflight, `Child`/`Output`. |
| `engine-proto` | `ContainerSpec`, `Request`/`Event`, frame codec (`u32 LE len | u8 kind | payload`). |
| `engine` | The single-threaded process: `engine-run` one-shot smoke verb + `engine serve --socket` accept loop. |
| `engine-client` | Tokio Unix-socket client + event pump for the daemon side (P2). |

The multicall binary dispatches `init-pro engine ...` / `engine-run`
before clap (`crates/init-pro/src/main.rs`), so the flag-strip filter
never sees them. `crates/multicall` has the `engine` alias.

## Kernel sequence

`clone3(child_stack = 0)` with
`CLONE_NEWNS|NEWPID|NEWUTS|NEWIPC|NEWCGROUP|NEWNET` and
`CLONE_INTO_CGROUP` when limits are requested. Child:
`MS_PRIVATE` → bind rootfs → enter → `/proc` `/sys` `/dev` → masked/ro
`/proc/sys` → `pivot_root` + `umount2(MNT_DETACH)` → hostname →
cap-bounding-set drop → optional `no_new_privs` → `setgid`/`setuid` →
`execve`. Parent opens a start gate, then `waitpid`s. Exit is
`128 + WTERMSIG`; exec failure is `126/127/255`.

Read-only bind mounts are hardened with `mount_setattr(2)`
(`AT_EMPTY_PATH|AT_RECURSIVE`, `RDONLY|NOSUID|NODEV|NOEXEC`). Note the
NOEXEC side effect: execution from a `:ro` bind is intentionally
denied. `mount_setattr` (Linux 5.12+) is a preflight requirement.

## Protocol

One frame is `u32 LE length | u8 kind | payload` (`MAX_FRAME` 16 MiB).
Kind 1 is JSON (`Request`/`Event`), 2/3/4 are raw stdout/stderr/stdin.
This is an internal byte channel; it is not governed by Q10 (the
JSON-only API wire format). One `Run` yields `Started`, zero or more
raw stdio frames, then `Exited`; `Ping` → `Pong`; `Shutdown` → `Bye`.

## Invariants

- `unsafe` only in `crates/nsrun`; every other crate keeps
  `#![forbid(unsafe_code)]`. Each nsrun unsafe block has a `SAFETY:`
  note and the crate denies `unsafe_op_in_unsafe_fn` +
  `clippy::undocumented_unsafe_blocks`.
- The engine process is single-threaded (no tokio, no `std::thread`):
  `clone3(child_stack = 0)` requires a fork-safe caller. `spawn` also
  checks `/proc/self/status` (`Threads: 1`) and refuses otherwise.
- Real root (no user namespace) and cgroup v2 only when limits are
  requested. On preflight failure P1's `engine serve` is a hard error
  (exit 1) - it never degrades silently; the containerd fallback is the
  T4.12 selection path (kubelet side decides after the engine refuses to
  start), not engine behaviour.
- Default backend remains `containerd`; `--container-engine=builtin`
  is wiring-deferred to T4.12.

## Operational semantics (P1, as shipped)

- **One connection, one container at a time.** The engine is
  single-threaded by design, so a peer that arrives while a container is
  running is answered with an `Event::Error` carrying
  `engine busy: one container at a time`, then closed - runs are never
  queued. Real container concurrency is T4.12's `EngineBackend` problem,
  not the engine's.
- **Timeouts.** The handshake read is bounded at 30 s and every socket
  write at 30 s, so a wedged peer cannot pin the loop forever. The idle
  accept park is a 20 ms poll.
- **Shutdown.** `SIGTERM`/`SIGINT`/`SIGHUP` are caught (they only set a
  flag; the loop polls it): the running container is killed, reaped, and
  its cgroup removed, then the engine exits 0. Measured latency is
  205–215 ms with a container running and 11–14 ms when idle.
- **Parent-death backstop.** The child arms `PR_SET_PDEATHSIG = SIGKILL`
  (re-armed after the credential switch), so a `SIGKILL`ed engine takes
  its container down in ~5 ms. That path leaks the cgroup directory -
  there is nothing left to remove it.
- **Socket ownership.** A live engine's socket is never unlinked: a
  second `engine serve` on the same path refuses with exit 1. Only a
  stale socket (dead owner, so `connect` fails) is replaced.
- **Exit codes.**

  | Code | Meaning |
  |------|---------|
  | 0 | success (also `engine-run --help`) |
  | 125 | container setup/exec-side failure after the start gate, message on fd 2 (also `engine-run` preflight failure) |
  | 126 | `execve` denied: `EACCES` / `EISDIR` |
  | 127 | exec target not found |
  | 128+signal | the container was killed by that signal |

  Usage errors exit 2; `engine serve` exits 1 on preflight failure,
  a live-socket refusal, or a bind failure.
- **`--cgroup` needs a limit.** `--cgroup NAME` (space or `=NAME` form)
  without at least one of `--memory-max` / `--cpu-max` / `--pids-max` is
  a hard usage error - `nsrun` skips the cgroup entirely without limits,
  so accepting the flag would be a silent no-op.

## Gates

- `scripts/unsafe-audit.sh` — unsafe boundary + SAFETY notes + lint gates.
- `scripts/engine-singlethread-check.sh` — dependency closure, no
  async/threads/spawn, pre-clap dispatch.
- `scripts/engine-selftest.sh` — E1–E8 on a real root host: exit codes,
  126/127, uid/gid/env/hostname, PID 1, cap bounding, no_new_privs,
  ro/rw binds, cgroup limits + cleanup, `--new-net`.
- `scripts/builtin-runtime-e2e.sh` — B1–B7 protocol: ping/pong, run
  streams + exit code, exec failure 127, cgroup over the wire, raw
  non-UTF8 stdout, malformed request, shutdown.
- All four are registered in `scripts/all-gates.sh` (Q48 static gates +
  domain e2e battery).

## Try it

```
cargo build
scripts/engine-selftest.sh
scripts/builtin-runtime-e2e.sh
# one-off:
target/debug/init-pro engine-run --rootfs /tmp/rootfs \
  --mount /usr/bin/busybox:/bin/busybox -- /bin/busybox sh -c 'echo hi'
```

The rootfs must contain `/bin/busybox` (a file target for the bind),
`/proc`, `/sys`, `/dev`; the engine creates the mounts. The host must
be real root, Linux 5.12+, and have cgroup v2 for limit cases.

## Roadmap

- **T4.12 (P2) — done:** `kubelet::EngineBackend` implementing
  `CriBackend`, exec relay over the frame protocol, and a real
  `--container-engine=builtin` selection with containerd fallback.
- **T4.13 (P3) — done:** Registry v2 pull + content-addressed layer
  store + per-container overlayfs rootfs (Q53/Q54); image config is the
  launch source of truth; quota + startup GC. Pod networking = host
  netns share (Q52).
- **T4.14 (P4) — done:** host-rootfs default + `init-pro.io/host-mounts`
  / `init-pro.io/rootfs` annotations (Q55); image chain is opt-in.
- **Open:** flannel/CNI conformance face rides T4.3 (host-gw,
  multi-node); the engine itself ships as an opt-in alternative to
  containerd.

## Attribution

Adapted from kiri (`github.com/libracoder/kiri`) at
`7846fe05c9b79c00cbc2f5a18d87bb1f74bf9191`, Apache-2.0,
Copyright (C) Jonas Schievink. See `NOTICE` and
`LICENSES/Apache-2.0.txt`; `link_repos/kiri` is a reference checkout
only (not a Cargo path dependency).
