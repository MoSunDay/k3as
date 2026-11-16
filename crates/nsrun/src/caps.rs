//! Capability bounding-set handling (T4.11, Q48). No `capset`: the
//! exec'd process gains only the bounding-set caps the image asked for.

use crate::sys;

/// Docker's default capability set (14 caps), as a bit mask.
pub const DEFAULT_CAPS: u64 = (1 << 0)   // CHOWN
    | (1 << 1)   // DAC_OVERRIDE
    | (1 << 3)   // FOWNER
    | (1 << 4)   // FSETID
    | (1 << 5)   // KILL
    | (1 << 6)   // SETGID
    | (1 << 7)   // SETUID
    | (1 << 8)   // SETPCAP
    | (1 << 10)  // NET_BIND_SERVICE
    | (1 << 13)  // NET_RAW
    | (1 << 18)  // SYS_CHROOT
    | (1 << 27)  // MKNOD
    | (1 << 29)  // AUDIT_WRITE
    | (1 << 31); // SETFCAP

/// Drops every kernel-known cap not in `allowed` from the bounding set.
/// EINVAL for caps this kernel does not know is deliberately ignored.
pub fn apply(allowed: u64) {
    for cap in 0..=sys::LAST_CAP {
        if cap < 64 && allowed & (1 << cap) == 0 {
            sys::drop_bounding_cap(cap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_docker_classic() {
        let count = DEFAULT_CAPS.count_ones();
        assert_eq!(count, 14);
        assert_eq!(DEFAULT_CAPS & (1 << 21), 0, "SYS_ADMIN stays out");
    }
}
