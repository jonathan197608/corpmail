//! secrets 模块单元测试（自 src/secrets.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;

fn conn() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);
         INSERT INTO settings VALUES ('enc:keyfile', '')",
    )
    .unwrap();
    c
}

#[test]
fn keyfile_roundtrip() {
    let c = conn();
    // 预置有效 keyfile key（避免单测触发 OS 随机写库分支的键冲突路径）
    let key: [u8; 32] = [7u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    c.execute("UPDATE settings SET v = ?1 WHERE k = 'enc:keyfile'", [b64])
        .unwrap();

    keyfile_store(&c, "demo@chinatelecom.cn", "测试密码123").unwrap();
    match keyfile_retrieve(&c, "demo@chinatelecom.cn") {
        GetResult::Keyfile(p) => assert_eq!(p, "测试密码123"),
        other => panic!("应走 keyfile 路径，实际 {other:?}"),
    }
}

#[test]
fn keyfile_missing_returns_missing() {
    let c = conn();
    assert!(matches!(keyfile_retrieve(&c, "nobody"), GetResult::Missing));
}

/// 缓存命中即返回：既不查 keychain 也不查库（库中没有任何凭据记录）。
/// 这条同时证明了弹窗主机路径被短路——retrieve 不再每次调用都过一遍
/// SecKeychainFindGenericPassword
#[test]
fn retrieve_hits_process_cache() {
    let account = "cache-hit@invalid.test";
    cache_put(account, GetResult::Keychain("pw-cached".into()));
    let c = conn();
    assert_eq!(
        retrieve(&c, account),
        GetResult::Keychain("pw-cached".into()),
        "命中缓存应直接返回"
    );
    cache_drop(account);
}

/// 缓存保留原始来源：Keyfile 值仍报 Keyfile，degraded 据此判定降级态
#[test]
fn degraded_follows_cached_source() {
    let account = "cache-degraded@invalid.test";
    cache_put(account, GetResult::Keyfile("pw-local".into()));
    let c = conn();
    assert!(degraded(&c, account), "keyfile 缓存应判为降级态");
    cache_drop(account);
}

/// clear 路径的缓存失效（不经 keyring，只验缓存原语）
#[test]
fn cache_drop_clears_entry() {
    let account = "cache-drop@invalid.test";
    cache_put(account, GetResult::Keyfile("pw".into()));
    assert!(cached(account).is_some());
    cache_drop(account);
    assert!(cached(account).is_none());
}
