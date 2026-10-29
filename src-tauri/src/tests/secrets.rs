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
