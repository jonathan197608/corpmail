//! secrets 模块单元测试（自 src/secrets.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）
//!
//! 注意：一律不触碰 keyring——沙箱内 keychain 写入会被拒且会卡住。回退路径
//! 直接调 keyfile_store/keyfile_retrieve，与生产分支同一份代码。

use super::*;
use std::path::PathBuf;

fn conn() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);")
        .unwrap();
    c
}

fn temp_key_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mbx-secrets-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(KEY_FILE)
}

fn cleanup(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn keyfile_roundtrip() {
    let c = conn();
    let kf = temp_key_path("roundtrip");
    keyfile_store(&c, &kf, "demo@chinatelecom.cn", "测试密码123").unwrap();
    match keyfile_retrieve(&c, &kf, "demo@chinatelecom.cn") {
        GetResult::Keyfile(p) => assert_eq!(p, "测试密码123"),
        other => panic!("应走 keyfile 路径，实际 {other:?}"),
    }
    cleanup(&kf);
}

#[test]
fn keyfile_missing_returns_missing() {
    let c = conn();
    let kf = temp_key_path("missing");
    assert!(matches!(
        keyfile_retrieve(&c, &kf, "nobody"),
        GetResult::Missing
    ));
    assert!(!kf.exists(), "只读操作不得凭空生成密钥文件");
    cleanup(&kf);
}

/// 密钥绝不入库：加密后 settings 表只应有密文行，没有任何密钥行。
/// 这是本次修复的核心断言——旧实现把 key 存在同一张表里，拿库即解密
#[test]
fn key_never_lands_in_database() {
    let c = conn();
    let kf = temp_key_path("no-db-key");
    keyfile_store(&c, &kf, "a@b.c", "pw").unwrap();
    assert!(kf.is_file(), "密钥应落在库外文件");

    let rows: Vec<String> = c
        .prepare("SELECT k FROM settings")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(
        rows,
        vec!["enc:a@b.c".to_string()],
        "不得出现密钥行：{rows:?}"
    );
    assert!(
        c.query_row(
            "SELECT count(*) FROM settings WHERE k = 'enc:keyfile'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap()
            == 0
    );
    cleanup(&kf);
}

/// 历史版本把密钥存在 settings.enc:keyfile（与密文同库）：
/// 首次访问应把密钥迁到文件、删除库内行，且**旧密文仍可解**
#[test]
fn legacy_db_key_migrates_to_file() {
    let c = conn();
    let kf = temp_key_path("legacy");
    let key: [u8; 32] = [9u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    c.execute("INSERT INTO settings VALUES ('enc:keyfile', ?1)", [&b64])
        .unwrap();
    // 用旧密钥加密一条密码（模拟历史数据）
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let nonce_bytes = [0u8; 12];
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), b"legacy-pw".as_ref())
        .unwrap();
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
    c.execute(
        "INSERT INTO settings VALUES ('enc:old@b.c', ?1)",
        [base64::engine::general_purpose::STANDARD.encode(blob)],
    )
    .unwrap();

    assert_eq!(
        keyfile_retrieve(&c, &kf, "old@b.c"),
        GetResult::Keyfile("legacy-pw".into()),
        "迁移后旧密文必须仍可解"
    );
    assert!(kf.is_file(), "密钥应已落盘");
    let left: i64 = c
        .query_row(
            "SELECT count(*) FROM settings WHERE k = 'enc:keyfile'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "密钥行应从库中移除");
    cleanup(&kf);
}

/// 密钥文件损坏：拒绝覆盖（覆盖 = 现有密文永久不可解），且明确报错
#[test]
fn corrupt_key_file_is_not_overwritten() {
    let c = conn();
    let kf = temp_key_path("corrupt");
    std::fs::write(&kf, b"this-is-not-a-key").unwrap();

    let err = keyfile_store(&c, &kf, "a@b.c", "pw").unwrap_err();
    assert!(err.contains("密钥"), "应说明密钥问题：{err}");
    assert_eq!(
        std::fs::read(&kf).unwrap(),
        b"this-is-not-a-key",
        "损坏的密钥文件必须原样保留"
    );
    cleanup(&kf);
}

#[cfg(unix)]
#[test]
fn key_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let c = conn();
    let kf = temp_key_path("perms");
    keyfile_store(&c, &kf, "a@b.c", "pw").unwrap();
    let mode = std::fs::metadata(&kf).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "密钥文件权限应为 0600，实际 {mode:o}");
    cleanup(&kf);
}

/// 缓存命中即返回：既不查 keychain 也不查库（库中没有任何凭据记录）。
/// 这条同时证明了弹窗主机路径被短路——retrieve 不再每次调用都过一遍
/// SecKeychainFindGenericPassword
#[test]
fn retrieve_hits_process_cache() {
    let account = "cache-hit@invalid.test";
    cache_put(account, GetResult::Keychain("pw-cached".into()));
    let c = conn();
    let kf = temp_key_path("cache-hit");
    assert_eq!(
        retrieve(&c, &kf, account),
        GetResult::Keychain("pw-cached".into()),
        "命中缓存应直接返回"
    );
    assert!(!kf.exists(), "缓存命中不应触碰密钥文件");
    cache_drop(account);
    cleanup(&kf);
}

/// 缓存保留原始来源：Keyfile 值仍报 Keyfile，degraded 据此判定降级态
#[test]
fn degraded_follows_cached_source() {
    let account = "cache-degraded@invalid.test";
    cache_put(account, GetResult::Keyfile("pw-local".into()));
    let c = conn();
    let kf = temp_key_path("cache-degraded");
    assert!(degraded(&c, &kf, account), "keyfile 缓存应判为降级态");
    cache_drop(account);
    cleanup(&kf);
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

/// 并发首启：多个线程同时要求同一把 key，必须只产生一个值。
/// 否则先写入的密文会被后一次 rename 换成另一把 key，密码永久不可解
#[test]
fn concurrent_key_creation_is_consistent() {
    let kf = temp_key_path("race");
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = kf.clone();
        handles.push(std::thread::spawn(move || {
            let c = conn();
            keyfile_key(&c, &path, true).unwrap()
        }));
    }
    let keys: Vec<[u8; 32]> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        keys.windows(2).all(|w| w[0] == w[1]),
        "并发创建必须收敛到同一把 key"
    );
    assert_eq!(
        read_key_file(&kf).unwrap().unwrap(),
        keys[0],
        "落盘 key 应与返回值一致"
    );
    cleanup(&kf);
}

/// 审计 R74：keychain 短路开关（原为编译期常量）改为可由环境变量显式恢复。
/// 只认 `CORPMAIL_KEYCHAIN=real`，其余取值一律保持默认短路——拼错时不能
/// 悄悄变成"走真实钥匙串"，那会在无人值守环境里卡住等授权
#[test]
fn skip_keychain_only_honours_exact_real_value() {
    assert!(!skip_keychain_for(Some("real")), "real 应恢复真实 keychain");
    assert!(skip_keychain_for(None), "默认按测试口径短路");
    assert!(skip_keychain_for(Some("")), "空串不生效");
    assert!(skip_keychain_for(Some("Real")), "大小写不匹配不生效");
    assert!(skip_keychain_for(Some("true")), "非 real 取值不生效");
    // 生产构建（非 test）默认不短路：由 cfg!(test) 决定，这里只断言二者一致
    assert_eq!(skip_keychain_for(None), cfg!(test));
}

/// C17：从 keyfile 降级态恢复到 keychain 成功后，库内旧密文必须删除。
///
/// 不清的后果：keychain 因 ad-hoc 签名 ACL 失配再次失效时，回退路径会读回
/// **改密前**的旧密码，发信/收件持续认证失败且 degraded() 不提示。
/// 走 finish_keychain_store 而非 store()，避免测试碰真实 keychain。
#[test]
fn keychain_recovery_purges_stale_keyfile_ciphertext() {
    let c = conn();
    let kf = temp_key_path("c17-recover");
    let account = "c17-recover@chinatelecom.cn";
    let other = "c17-other@chinatelecom.cn";
    keyfile_store(&c, &kf, account, "旧密码").unwrap();
    keyfile_store(&c, &kf, other, "别人的密码").unwrap();
    // 前置：回退路径本来确实能读回旧密码（否则「删掉」无鉴别力）
    assert_eq!(
        keyfile_retrieve(&c, &kf, account),
        GetResult::Keyfile("旧密码".into())
    );

    let degraded = finish_keychain_store(&c, account, "新密码").unwrap();
    assert!(!degraded, "keychain 成功不应报降级");
    assert!(
        matches!(keyfile_retrieve(&c, &kf, account), GetResult::Missing),
        "本账号旧密文行必须删除"
    );
    assert_eq!(
        keyfile_retrieve(&c, &kf, other),
        GetResult::Keyfile("别人的密码".into()),
        "其它账号密文不得被牵连"
    );
    assert!(
        matches!(retrieve_fallback(&c, &kf, account), GetResult::Missing),
        "keychain 再次失效时不得读回旧密码"
    );
    assert_eq!(
        cached(account),
        Some(GetResult::Keychain("新密码".into())),
        "缓存应刷新为新密码"
    );
    // 全局进程内缓存：用独立账号 + 显式清理，避免串扰其它测试
    cache_drop(account);
    cache_drop(other);
    cleanup(&kf);
}

/// 删除失败必须上报：keychain 已写成功但旧密文还在，静默 Ok 会留下坏态
#[test]
fn keychain_recovery_reports_purge_failure() {
    let c = Connection::open_in_memory().unwrap(); // 故意不建 settings 表
    let account = "c17-nosettings@chinatelecom.cn";
    let err = finish_keychain_store(&c, account, "新密码").unwrap_err();
    assert!(err.contains("清理本地旧密码失败"), "错误应说明真实原因：{err}");
    assert_eq!(cached(account), None, "失败时不得把新密码写进缓存");
    cache_drop(account);
}
