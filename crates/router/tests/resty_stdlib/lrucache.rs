//! `resty.lrucache` acceptance (5 stateless sync tests).

use super::helpers::eval;

// ============================ resty.lrucache ============================

#[test]
fn lrucache_set_get_and_nil() {
    let got: String = eval("local c = resty.lrucache.new(8) c:set('k','v') return c:get('k')");
    assert_eq!(got, "v");
    let miss: Option<String> = eval("local c = resty.lrucache.new(8) return c:get('nope')");
    assert!(miss.is_none());
}

#[test]
fn lrucache_evicts_lru_at_capacity() {
    // cap 2: insert a,b,c -> a evicted; b,c survive.
    let (a, b, c): (Option<String>, String, String) = eval(
        "local c = resty.lrucache.new(2) \
         c:set('a','A') c:set('b','B') c:set('c','C') \
         return c:get('a'), c:get('b'), c:get('c')",
    );
    assert!(a.is_none());
    assert_eq!((b.as_str(), c.as_str()), ("B", "C"));
}

#[test]
fn lrucache_get_touches_recency() {
    // cap 2: a,b; read a (touches); insert c -> b evicted, a survives.
    let (a, b, c): (String, Option<String>, String) = eval(
        "local c = resty.lrucache.new(2) \
         c:set('a','A') c:set('b','B') local _=c:get('a') c:set('c','C') \
         return c:get('a'), c:get('b'), c:get('c')",
    );
    assert_eq!(a, "A");
    assert!(b.is_none());
    assert_eq!(c, "C");
}

#[test]
fn lrucache_count_get_keys_delete_flush() {
    let (n, keys): (i64, Vec<String>) = eval(
        "local c = resty.lrucache.new(8) c:set('x',1) c:set('y',2) \
         return c:count(), c:get_keys()",
    );
    assert_eq!(n, 2);
    assert_eq!(keys.len(), 2);
    let (after_del, after_flush): (Option<i64>, i64) = eval(
        "local c = resty.lrucache.new(8) c:set('k',9) c:delete('k') \
         local a=c:get('k') c:set('k2',1) c:flush_all() return a, c:count()",
    );
    assert!(after_del.is_none());
    assert_eq!(after_flush, 0);
}

#[test]
fn lrucache_stores_table_values() {
    let n: i64 = eval("local c = resty.lrucache.new(4) c:set('t',{1,2,3}) return #c:get('t')");
    assert_eq!(n, 3);
}
