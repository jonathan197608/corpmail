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
    assert!(
        err.contains("清理本地旧密码失败"),
        "错误应说明真实原因：{err}"
    );
    assert_eq!(cached(account), None, "失败时不得把新密码写进缓存");
    cache_drop(account);
}

// ---------- A1：改密回退 keyfile 前清 keychain 旧条目 ----------

/// A1 正向：keychain 写失败 → 删旧条目成功 → 回退 keyfile。
/// 旧实现不删旧条目：keychain 里仍是 P1，重启后两段式读取（keychain 优先）
/// 读回 P1，改密静默失效
#[test]
fn store_fallback_deletes_stale_keychain_entry() {
    let c = conn();
    let kf = temp_key_path("a1-fallback");
    let account = "a1-fallback@chinatelecom.cn";
    let g = fake_keyring::Guard::install();
    g.preset(account, "P1");
    g.make_set_fail();
    cache_drop(account); // 隔离全局缓存

    let degraded = store(&c, &kf, account, "P2").unwrap();
    assert!(degraded, "回退 keyfile 应报降级");
    // keychain 旧条目必须已清：否则重启后 Keychain(P1) 永远压过 enc: 的 P2
    assert_eq!(g.entry(account), None, "keychain 旧密码 P1 必须已清除");
    // 重启语义（cache 模拟进程内存）：丢弃缓存后读回 Keyfile(P2)
    cache_drop(account);
    assert_eq!(
        retrieve(&c, &kf, account),
        GetResult::Keyfile("P2".into()),
        "旧条目已删，重启后应读回 enc: 里的新密码"
    );
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// A1 防半保存态：keychain 不可写且旧条目删不掉时，改密必须明确报 Err，
/// 且 **不写 enc:**（不制造「看似保存成功、重启后失效」的坏态）。
/// 这是设计取舍：明确失败让用户解锁钥匙串后重试，好过静默双存新旧密码
#[test]
fn store_fails_loudly_when_stale_keychain_undeletable() {
    let c = conn();
    let kf = temp_key_path("a1-undeletable");
    let account = "a1-undeletable@chinatelecom.cn";
    let g = fake_keyring::Guard::install();
    g.preset(account, "P1");
    g.make_set_fail();
    g.make_delete_fail();
    cache_drop(account);

    let err = store(&c, &kf, account, "P2").unwrap_err();
    assert!(err.contains("无法清除"), "错误应说明根因：{err}");
    // enc: 不得写入 P2
    assert!(
        matches!(keyfile_retrieve(&c, &kf, account), GetResult::Missing),
        "失败路径不得写入 enc: 密文（半保存态）"
    );
    assert_eq!(g.entry(account), Some("P1".into()), "keychain 保持 P1 原样");
    // 重启语义：读回 Keychain(P1)——明确失败优于静默复活 P2 假象
    cache_drop(account);
    assert_eq!(retrieve(&c, &kf, account), GetResult::Keychain("P1".into()));
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// store 的 keychain 成功分支首次在单测中被真实执行（此前只有
/// finish_keychain_store 的直调测试 C17）：写 keychain 成功 + 清 enc: 旧密文
#[test]
fn store_keychain_success_purges_keyfile_ciphertext() {
    let c = conn();
    let kf = temp_key_path("a1-success");
    let account = "a1-success@chinatelecom.cn";
    // 前置：keyfile 降级态（旧密码在 enc:）
    keyfile_store(&c, &kf, account, "旧密码").unwrap();
    let g = fake_keyring::Guard::install();
    cache_drop(account);

    let degraded = store(&c, &kf, account, "P2").unwrap();
    assert!(!degraded, "keychain 写成功不应报降级");
    assert_eq!(g.entry(account), Some("P2".into()), "keychain 应存新密码");
    assert!(
        matches!(keyfile_retrieve(&c, &kf, account), GetResult::Missing),
        "enc: 旧密文必须清除（C17 语义经 store 主链复验）"
    );
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// A1 环境级失败：keychain 后端完全不可用（set 失败 + 无条目可读，
/// 如 e2e 隔离 HOME 无默认钥匙串）。旧密码此刻读不到 → 重启后 keychain_get
/// 同样 miss → 无复活路径，必须放行 keyfile 回退（回归 e2e 向导保存）
#[test]
fn store_falls_back_when_keychain_backend_unavailable() {
    let c = conn();
    let kf = temp_key_path("a1-nobackend");
    let account = "a1-nobackend@chinatelecom.cn";
    let g = fake_keyring::Guard::install();
    g.make_set_fail(); // 未 preset：get_password → None（相当于无默认钥匙串）
    g.make_delete_backend_absent(); // delete 报 "default keychain could not be found"（E2E 实测形态）
    cache_drop(account);

    let degraded = store(&c, &kf, account, "P2").unwrap();
    assert!(
        degraded,
        "后端不可用应顺利回退 keyfile（不制造「降级失败」假象）"
    );
    assert_eq!(
        retrieve(&c, &kf, account),
        GetResult::Keyfile("P2".into()),
        "回退后本进程读 Keyfile(P2)"
    );
    cache_drop(account);
    assert_eq!(
        retrieve(&c, &kf, account),
        GetResult::Keyfile("P2".into()),
        "重启语义：keychain 读不到（无条目）→ enc: 的 P2"
    );
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// A1 / 复查 R1：旧条目**在**但当下读不到（钥匙串锁定 / ACL 拒读，get→None）
/// 且删不掉时，必须返回 Err 且**不写 enc:**。否则静默双写，重启解锁后
/// retrieve 走 keychain 优先读回 P1，新密码 P2 永不生效、degraded 无提示——
/// 与原始 bug 同态。旧实现用 `get().is_some()` 判断复活路径，会把「条目在
/// 但读不到」误判成「无条目」而跳过删除，正是本用例要钉死的偏差。
#[test]
fn store_fails_when_old_entry_unreadable_and_undeletable() {
    let c = conn();
    let kf = temp_key_path("a1-unreadable");
    let account = "a1-unreadable@chinatelecom.cn";
    let g = fake_keyring::Guard::install();
    g.preset(account, "P1"); // 旧条目确实在
    g.make_set_fail(); // 写 P2 失败 → 走回退决策
    g.make_get_fail(); // 但当下读不到（模拟钥匙串锁定）
    g.make_delete_fail(); // 且删不掉 → 必须 Err，不得写 enc:
    cache_drop(account);

    let result = store(&c, &kf, account, "P2");
    assert!(result.is_err(), "旧条目在但删不掉必须 Err，不得静默双写");
    assert!(
        matches!(keyfile_retrieve(&c, &kf, account), GetResult::Missing),
        "Err 分支绝不能写 enc:（否则重启解锁后读回 P1）"
    );
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// 复查 R1：后端「根本不存在」（放行）与「存在但删不掉」（上抛）必须分类正确。
/// 前者是 e2e 隔离 HOME 实测形态（首次保存不能挡），后者是原始 bug 同态
/// （钥匙串锁定/ACL 拒删，写 enc: 会埋下重启复活旧密码的坏态）。
#[test]
fn keychain_backend_absent_classifies_only_absent_storage() {
    // 放行：后端根本不存在（macOS 无默认 keychain / keyring 无默认 store）
    for msg in [
        "Platform failure: A default keychain could not be found.",
        "SecKeychainCopyDefault: a default keychain could not be found",
        "No default store has been set",
        // 复查 N4：keyring 4.2 `Entry::new` 在初始化失败时返回 NoDefaultStore，
        // Display 即下面这段文本（v1.rs:47-50）。该错误现在也走 normalize_delete，
        // 必须放行——否则分类器里的 "no default store" 分支在删除路径不可达
        "No default store has been set, so cannot search or create entries",
    ] {
        assert!(keychain_backend_absent(msg), "应识别为后端不存在：{msg}");
    }
    // 上抛：后端存在但被锁 / ACL 拒绝 —— 此时旧条目可能存在，不能放行
    for msg in [
        "Couldn't access platform storage: The keychain was locked",
        "Platform failure: User interaction is not allowed",
        "注入的 keychain 删除失败",
        "",
    ] {
        assert!(!keychain_backend_absent(msg), "不得误判为后端不存在：{msg}");
    }
    // 归一：后端不存在 → Ok(false)；其它错误原样上抛
    assert_eq!(normalize_delete(Err("a default keychain could not be found".into())), Ok(false));
    assert!(normalize_delete(Err("keychain was locked".into())).is_err());
    assert_eq!(normalize_delete(Ok(true)), Ok(true));
}

#[test]
fn p62_degraded_three_stage_functions() {
    // 第六轮 P2-2：三段判定纯函数各自口径正确，供 state 两段式编排调用
    let account = "p62@x.cn";
    let c = conn();
    let kf = temp_key_path("p62-degraded");

    // 第三段：无 keyfile 行 → 非降级
    assert!(!degraded_keyfile_only(&c, &kf, account));
    // 写入 keyfile 密文 → 降级态
    keyfile_store(&c, &kf, account, "pw-p62").unwrap();
    assert!(degraded_keyfile_only(&c, &kf, account));

    // 第一段：缓存命中直接给出结论，不看库
    // （cache 为全局静态，用独立账号避免与其它测试串味）
    let cache_account = "p62-cache@x.cn";
    assert_eq!(degraded_from_cache(cache_account), None);
    cache_put(cache_account, GetResult::Keyfile("x".into()));
    assert_eq!(degraded_from_cache(cache_account), Some(true));
    cache_put(cache_account, GetResult::Keychain("x".into()));
    assert_eq!(degraded_from_cache(cache_account), Some(false));
    cache_drop(cache_account);

    cleanup(&kf);
}

// ---------- R14：密钥回退顺序与 keyfile 缺失保护 ----------

/// R14 P1：keychain set 失败 + 旧条目删除成功后，keyfile 写入失败时
/// 旧实现会把旧密码删掉且新密码未落 → 账号彻底无凭据。
/// 新实现先写 enc: 再删旧条目，写失败时旧密码原样保留。
#[test]
fn r14_failed_keyfile_write_keeps_old_keychain_password() {
    let c = conn();
    let kf = temp_key_path("r14-write-fail");
    let account = "r14-write-fail@chinatelecom.cn";
    // 损坏的密钥文件 → keyfile_store 必失败
    std::fs::write(&kf, b"this-is-not-a-key").unwrap();
    let g = fake_keyring::Guard::install();
    g.preset(account, "P1");
    g.make_set_fail();
    cache_drop(account);

    let err = store(&c, &kf, account, "P2").unwrap_err();
    assert!(err.contains("回退密钥保存失败"), "错误应说明写失败：{err}");
    assert_eq!(
        g.entry(account),
        Some("P1".into()),
        "写失败时 keychain 旧密码必须保留（旧实现先删导致永久丢失）"
    );
    cache_drop(account);
    assert_eq!(
        retrieve(&c, &kf, account),
        GetResult::Keychain("P1".into()),
        "重启后仍可读回旧密码"
    );
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// R14 P1 配套：写 enc: 成功但删 keychain 失败时，必须回滚刚写入的
/// 新密文，使 keychain 里的旧密码保持权威（不制造静默双写）。
#[test]
fn r14_failed_keychain_delete_rolls_back_new_ciphertext() {
    let c = conn();
    let kf = temp_key_path("r14-delete-fail");
    let account = "r14-delete-fail@chinatelecom.cn";
    let g = fake_keyring::Guard::install();
    g.preset(account, "P1");
    g.make_set_fail();
    g.make_delete_fail();
    cache_drop(account);

    let err = store(&c, &kf, account, "P2").unwrap_err();
    assert!(err.contains("无法清除"), "错误应说明删除失败：{err}");
    assert_eq!(g.entry(account), Some("P1".into()), "旧条目保留");
    assert!(
        matches!(keyfile_retrieve(&c, &kf, account), GetResult::Missing),
        "删除失败必须回滚本次写入的 enc:（重启读回 P1）"
    );
    drop(g);
    cache_drop(account);
    cleanup(&kf);
}

/// R14 P2：密钥文件丢失但库中已有 enc: 密文时，拒绝静默生成新密钥
/// （旧实现生成新 key 后所有旧密文永久不可解，且无任何提示）。
#[test]
fn r14_missing_key_file_refuses_new_key_when_ciphertext_exists() {
    let c = conn();
    let kf = temp_key_path("r14-missing-key");
    let first = "r14-missing-first@chinatelecom.cn";
    let second = "r14-missing-second@chinatelecom.cn";
    keyfile_store(&c, &kf, first, "pw1").unwrap();
    std::fs::remove_file(&kf).unwrap();

    let err = keyfile_store(&c, &kf, second, "pw2").unwrap_err();
    assert!(
        err.contains("拒绝生成新密钥"),
        "应明确拒绝而非静默生成：{err}"
    );
    assert!(!kf.exists(), "失败路径不得留下新密钥文件");
    assert!(
        matches!(db::settings::get(&c, &format!("enc:{second}")), Ok(None)),
        "失败路径不得写入新账号密文"
    );
    assert!(
        matches!(db::settings::get(&c, &format!("enc:{first}")), Ok(Some(_))),
        "旧密文必须仍在库中（找回原 key 文件后可解）"
    );
    cleanup(&kf);
}
