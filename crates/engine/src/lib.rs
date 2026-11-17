//! `engine` — the built-in container engine process (TODO **T4.11**, Q48).
//!
//! Single-threaded by construction: no tokio, no `std::thread`, no async
//! runtime. The daemon re-execs `init-pro engine serve --socket ...`; the
//! [`nsrun`] child is cloned from this process, so `clone3(child_stack=0)`
//! stays fork-safe. `scripts/engine-singlethread-check.sh` enforces it.
//!
//! This slice (P1) ships the kernel + the one-shot smoke verb
//! (`init-pro engine-run`). Image store, CNI, CRI shims and kubelet wiring
//! are T4.12/T4.13/P2+ work; the protocol below is designed for them.

#![forbid(unsafe_code)]

mod cli;
mod convert;
mod exec;
mod logs;
mod server;
mod session;
mod wire;

/// Dispatch `engine <verb>` / `engine-run`. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("engine-run") => cli::run_once(&args[1..]),
        Some("serve") => cli::run_serve(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            print_usage();
            0
        }
        Some(other) => {
            eprintln!("engine: unknown command {other:?}");
            print_usage();
            2
        }
    }
}

fn print_usage() {
    println!(
        "Built-in container engine (internal; Q48)\n\
         \n\
         init-pro engine-run --rootfs DIR [--hostname NAME] [--uid N] [--gid N]\n\
         \x20   [--env K=V]... [--mount SRC:DST[:ro]]... [--new-net]\n\
         \x20   [--netns /proc/PID/ns/net] [--memory-max BYTES]\n\
         \x20   [--cpu-max QUOTA_PERIOD] [--pids-max N]\n\
         \x20   [--cgroup NAME] [--] COMMAND [ARG]...\n\
         \n\
         init-pro engine serve --socket PATH [--pause] [--netns PATH]\n\
         \x20   [--log-path PATH] [--mount SRC:DST[:ro]]...\n\
         \n\
         The engine must run as real root on a single-threaded process; it\n\
         needs cgroup v2 only when limits are requested. --cgroup NAME only\n\
         names the cgroup that holds limits, so it is an error on its own.\n\
         Preflight failure is a hard error today (T4.12 wires the containerd\n\
         fallback)."
    );
}
