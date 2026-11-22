//! IPv4 CIDR arithmetic for the NetworkPolicy compiler (TODO **T4.4**,
//! decision **Q64**): `ipBlock.cidr` minus `ipBlock.except`, rendered as
//! canonical CIDRs.
//!
//! Pure math over `(u32, u32)` inclusive ranges; parsing is delegated to
//! the shared `api::ipv4_cidr_range` (one parser, admission and
//! enforcement agree — Q64). Split out of `netpol_compile.rs` to keep
//! both files under the 400-line cap.

use api::ipv4_cidr_range;

/// `cidr` minus every `except` CIDR, as canonical `a.b.c.d/p` strings.
/// Unparseable base: emitted verbatim (fail-closed — the rule still
/// matches a literal); unparseable `except` entries are ignored
/// (admission already rejected them, Q64).
pub(crate) fn subtract_cidr(cidr: &str, except: &[String]) -> Vec<String> {
    let Some((base_lo, base_hi)) = ipv4_cidr_range(cidr) else {
        return vec![cidr.to_string()];
    };
    // Clip every hole to the base range, then merge overlaps/adjacency.
    let mut holes: Vec<(u32, u32)> = except
        .iter()
        .filter_map(|e| ipv4_cidr_range(e))
        .filter(|(lo, hi)| *hi >= base_lo && *lo <= base_hi)
        .map(|(lo, hi)| (lo.max(base_lo), hi.min(base_hi)))
        .collect();
    holes.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (lo, hi) in holes {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    let mut cursor = base_lo;
    let mut out = Vec::new();
    for (lo, hi) in merged {
        if lo > cursor {
            out.extend(range_to_cidrs(cursor, lo - 1));
        }
        cursor = hi.saturating_add(1);
    }
    if cursor <= base_hi {
        out.extend(range_to_cidrs(cursor, base_hi));
    }
    out
}

/// Greedy aligned-prefix split of an inclusive IPv4 range into the
/// minimal set of canonical CIDRs (largest block first).
pub(crate) fn range_to_cidrs(lo: u32, hi: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = lo;
    while cur <= hi {
        let align = cur.trailing_zeros() as u64; // low zero bits (cur may be 0)
        let span = hi as u64 - cur as u64 + 1;
        let span_bits = (63 - span.leading_zeros()) as u64; // floor(log2(span)), 0..=32
        let width = align.min(span_bits);
        if width == 32 {
            out.push("0.0.0.0/0".to_string()); // whole address space
            break;
        }
        out.push(format!("{}/{}", u32_to_ip(cur), 32 - width as u32));
        cur += 1u32 << width;
    }
    out
}

fn u32_to_ip(v: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        v >> 24,
        (v >> 16) & 0xff,
        (v >> 8) & 0xff,
        v & 0xff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn range_splits_are_canonical() {
        assert_eq!(range_to_cidrs(5, 5), v(&["0.0.0.5/32"]));
        // 10.0.0.0-10.0.0.255 is one aligned /24
        assert_eq!(
            range_to_cidrs(0x0A00_0000, 0x0A00_00FF),
            v(&["10.0.0.0/24"])
        );
        // 5..7 -> /32 + /31 (alignment splits the low end)
        assert_eq!(
            range_to_cidrs(0x0A00_0005, 0x0A00_0007),
            v(&["10.0.0.5/32", "10.0.0.6/31"])
        );
        // whole space collapses to /0
        assert_eq!(range_to_cidrs(0, u32::MAX), v(&["0.0.0.0/0"]));
        assert!(range_to_cidrs(7, 5).is_empty()); // empty range
    }

    #[test]
    fn subtract_removes_holes() {
        // /24 minus a /32 and the top half
        assert_eq!(
            subtract_cidr("10.0.0.0/24", &v(&["10.0.0.9/32", "10.0.0.128/25"])),
            v(&[
                "10.0.0.0/29",
                "10.0.0.8/32",
                "10.0.0.10/31",
                "10.0.0.12/30",
                "10.0.0.16/28",
                "10.0.0.32/27",
                "10.0.0.64/26"
            ])
        );
        // fully carved out -> nothing left
        assert!(subtract_cidr("10.0.0.0/24", &v(&["10.0.0.0/24"])).is_empty());
        // adjacent holes merge into one split: 64-191 carved out leaves
        // /26 + /26 (a /25 would cover the hole)
        assert_eq!(
            subtract_cidr("10.0.0.0/24", &v(&["10.0.0.64/26", "10.0.0.128/26"])),
            v(&["10.0.0.0/26", "10.0.0.192/26"])
        );
        // out-of-range holes ignored; invalid base passes through
        assert_eq!(
            subtract_cidr("10.0.0.0/24", &v(&["192.168.1.0/24"])),
            v(&["10.0.0.0/24"])
        );
        assert_eq!(subtract_cidr("garbage", &[]), v(&["garbage"]));
    }
}
