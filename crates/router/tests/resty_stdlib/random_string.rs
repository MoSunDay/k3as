//! `resty.random`, `resty.sha256`, and `resty.string` acceptance (6 stateless
//! sync tests).

use router::worker_vm;

use super::helpers::eval;

// ============================ resty.random ============================

#[test]
fn random_bytes_exact_length() {
    let lua = worker_vm().expect("vm");
    let b: mlua::LuaString =
        lua.load("return resty.random.bytes(32)").eval().expect("eval");
    assert_eq!(b.as_bytes().len(), 32);
    assert!(b.as_bytes().iter().any(|&x| x != 0));
}

#[test]
fn random_token_is_urlsafe_and_unique() {
    let (a, b): (String, String) =
        eval("return resty.random.token(24), resty.random.token(24)");
    for t in [&a, &b] {
        assert!(!t.is_empty());
        assert!(
            t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "non-urlsafe char in {t}"
        );
    }
    assert_ne!(a, b);
}

// ============================ resty.sha256 ============================

#[test]
fn sha256_known_and_empty_vectors() {
    let h: String =
        eval("local d = resty.sha256:new() d:update('abc') return d:final()");
    assert_eq!(
        h,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let e: String = eval("return resty.sha256:new():final()");
    assert_eq!(
        e,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_call_styles_agree() {
    let (a, b): (String, String) = eval(
        "local f = function() local d = resty.sha256.new() d:update('hi') return d:final() end \
         local g = function() local d = resty.sha256:new() d:update('hi') return d:final() end \
         return f(), g()",
    );
    assert_eq!(a, b);
}

// ============================ resty.string ============================

#[test]
fn base64_roundtrip_and_nopad() {
    let (padded, nopad, dec): (String, String, String) = eval(
        "return resty.string.encode_base64('hello world'), \
                resty.string.encode_base64('hello world', true), \
                resty.string.decode_base64('aGVsbG8gd29ybGQ=')",
    );
    assert_eq!(padded, "aGVsbG8gd29ybGQ=");
    assert_eq!(nopad, "aGVsbG8gd29ybGQ");
    assert_eq!(dec, "hello world");
}

#[test]
fn hex_roundtrip() {
    let (enc, dec): (String, String) =
        eval("return resty.string.to_hex('hello'), resty.string.from_hex('68656c6c6f')");
    assert_eq!(enc, "68656c6c6f");
    assert_eq!(dec, "hello");
}
