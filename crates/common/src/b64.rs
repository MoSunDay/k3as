//! Standard base64 (RFC 4648, padded) codec — pure functions, no deps.
//!
//! T4.5 (Q65): the reverse relay tunnel carries kubelet request/response
//! BODIES inside JSON envelopes (Q10 JSON-only wire). Kubelet log/exec
//! bodies are not guaranteed UTF-8, so they travel base64-encoded under
//! the `bodyB64` field instead of as JSON strings — lossless byte
//! pass-through keeps decision Q31's verbatim-body contract.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `data` as padded standard base64.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn value_of(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode padded standard base64; `Err` carries the offending position.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "base64 length {} is not a multiple of 4",
            bytes.len()
        ));
    }
    // Only the final quartet may carry padding.
    let pad = bytes.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return Err("base64 has more than two padding chars".to_string());
    }
    if bytes[..bytes.len() - pad].contains(&b'=') {
        return Err("base64 padding in the middle of the input".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, quartet) in bytes.chunks(4).enumerate() {
        let mut n: u32 = 0;
        for (j, &c) in quartet.iter().enumerate() {
            let v = if c == b'=' {
                if i * 4 + j < bytes.len() - pad {
                    return Err("base64 padding in the middle of the input".to_string());
                }
                0
            } else {
                value_of(c).ok_or_else(|| {
                    format!(
                        "base64 byte {c:?} at offset {} is not in the alphabet",
                        i * 4 + j
                    )
                })?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if quartet[2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if quartet[3] != b'=' {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        for (raw, enc) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw), enc, "encode {raw:?}");
            assert_eq!(decode(enc).unwrap(), raw, "decode {enc:?}");
        }
    }

    #[test]
    fn all_byte_values_round_trip() {
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&data)).unwrap(), data);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode("A").is_err(), "length");
        assert!(decode("====").is_err(), "too much padding");
        assert!(decode("AB=C").is_err(), "mid padding");
        assert!(decode("AB!C").is_err(), "bad alphabet");
    }
}
