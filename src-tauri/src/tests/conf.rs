//! conf 模块单元测试（自 src/conf.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::state::keys;

/// 测试便捷：读取成功即展开。错误路径用 `super::load` 直接断言 Err。
fn load(p: &Path) -> BTreeMap<String, String> {
    super::load(p).expect("配置读取失败")
}

#[test]
fn save_load_roundtrip_and_corrupt_tolerated() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = BTreeMap::new();
    cfg.insert(keys::DATA_DIR.to_string(), "/Volumes/data/mail".to_string());
    cfg.insert(
        keys::DISPLAY_NAME.to_string(),
        "张三 \"引号\" = 等号".to_string(),
    );
    save(&dir, &cfg).unwrap();
    let back = load(&dir);
    assert_eq!(back.get(keys::DATA_DIR).unwrap(), "/Volumes/data/mail");
    // 特殊字符 roundtrip 无损
    assert_eq!(
        back.get(keys::DISPLAY_NAME).unwrap(),
        "张三 \"引号\" = 等号"
    );
    // 损坏文件 → 空 map（不 panic，自愈路径）
    std::fs::write(dir.join(CONFIG_FILE), "not [ valid toml ===").unwrap();
    assert!(load(&dir).is_empty());
    // 未知键被忽略（只承载 CONFIG_KEYS）
    std::fs::write(
        dir.join(CONFIG_FILE),
        format!("{} = \"x\"\nunknown_key = \"y\"\n", keys::SMTP_HOST),
    )
    .unwrap();
    let back2 = load(&dir);
    assert_eq!(back2.len(), 1, "未知键应被忽略：{back2:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 配置文件损坏：原文改名留证，绝不让后续保存覆盖唯一可恢复的副本
#[test]
fn corrupt_config_is_backed_up() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-broken-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(CONFIG_FILE), "not [ valid toml ===").unwrap();

    assert!(load(&dir).is_empty(), "损坏时应降级为未配置");
    assert!(!dir.join(CONFIG_FILE).exists(), "被覆盖前应已从原位移走");
    let backup = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("corpmail.toml.broken-")
        })
        .expect("应留下损坏原文备份");
    let text = std::fs::read_to_string(backup.path()).unwrap();
    assert_eq!(text, "not [ valid toml ===", "备份应是原文");
    let _ = std::fs::remove_dir_all(&dir);
}

/// C16：解析失败且**原文备份也失败**时不得按「未配置」继续。
///
/// 旧实现无条件 `Ok(BTreeMap::new())`，但原文仍留在原位且可能含
/// data_dir_override；向导保存会原子覆盖它，用户唯一的目录指针就没了。
/// 用「预占备份名」制造 rename 失败：无需改权限，跨平台稳定。
/// 断言必须落在终态文件上（原文仍在、内容未变），不能只断言返回 Err。
#[test]
fn corrupt_config_without_backup_fails_loudly_and_keeps_original() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-nobackup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 损坏但含有效目录指针的原文：正是「静默按未配置启动」会毁掉的那一份
    let original = format!(
        "{} = \"/Volumes/data/mail\"\nthis is not [ valid toml ===",
        keys::DATA_DIR
    );
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, &original).unwrap();

    // R17：备份名改为「秒-纳秒(9 位)-pid」后，无法再靠预建秒级目录撞名。
    // 用可注入时间戳的测试缝锁定精确候选名，再预建成目录：rename(文件 →
    // 已存在目录) 在 Unix/Windows 都失败，跨平台且不依赖权限。
    let stamp = (1_700_000_000u64, 123_456_789u32);
    let backup = format!(
        "{CONFIG_FILE}.broken-{}-{:09}-{}",
        stamp.0,
        stamp.1,
        std::process::id()
    );
    std::fs::create_dir_all(dir.join(&backup)).unwrap();

    // max_candidates=1：唯一候选已被目录占用，无后缀可退，rename 必失败
    let err = broken_fallback_at(&dir, &path, "测试预占备份名", stamp, 1)
        .expect_err("备份失败时必须明确报错，不能静默按未配置启动");
    assert!(err.contains("备份失败"), "错误应说明真实原因：{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "原文必须原样保留，等待用户处理"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_uses_pid_scoped_tmp_and_does_not_touch_legacy_stub() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let legacy = dir.join(format!("{CONFIG_FILE}.tmp"));
    std::fs::write(&legacy, b"legacy").unwrap();
    let mut cfg = BTreeMap::new();
    cfg.insert(keys::DISPLAY_NAME.to_string(), "张三".to_string());
    save(&dir, &cfg).unwrap();
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        b"legacy",
        "pid 命名的 tmp 不应覆盖旧固定名残留"
    );
    assert!(
        !dir.join(format!("{CONFIG_FILE}.{}.tmp", std::process::id()))
            .exists(),
        "rename 成功后临时文件必须消失"
    );
    assert_eq!(
        load(&dir).get(keys::DISPLAY_NAME).map(String::as_str),
        Some("张三")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R17 P2：save 的任何失败路径都不能留下 pid tmp。把目标配置路径预建成目录，
/// 让 rename 稳定失败（Unix/Windows 同口径），断言 tmp 已被统一清理。
#[test]
fn r17_save_failure_removes_pid_scoped_tmp() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-r17-tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(CONFIG_FILE)).unwrap();
    let mut cfg = BTreeMap::new();
    cfg.insert(keys::DISPLAY_NAME.to_string(), "张三".to_string());
    let err = save(&dir, &cfg).expect_err("目标路径是目录时 rename 必须失败");
    assert!(err.contains("配置落盘失败"), "错误应指向落盘阶段：{err}");
    assert!(
        !dir.join(format!("{CONFIG_FILE}.{}.tmp", std::process::id()))
            .exists(),
        "任何失败路径都不得残留 tmp（旧实现 write/sync 失败时仍残留）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- A8（audit 2026-11-16）：非 UTF-8 配置与语法错同走备份自愈 ----------

/// A8：含非法 UTF-8 字节的 corpmail.toml 不得致命 Err 启动失败
/// （read_to_string 的 InvalidData 旧路径），必须与 TOML 语法错同走
/// 备份 → 按未配置继续
#[test]
fn non_utf8_config_backs_up_and_starts_unconfigured() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-a8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 不带 BOM 的纯 Latin-1 字节（0xFF 非法）
    let original: &[u8] = b"account_prefix = \"caf\xFF\"\n";
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, original).unwrap();

    let map = super::load(&dir).expect("编码损坏应自愈为未配置，不得挡启动");
    assert!(map.is_empty(), "按未配置继续：{map:?}");

    // 备份存在且与原文逐字节一致（含非法字节本身）
    let mut backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_string().unwrap())
        .filter(|n| n.starts_with(&format!("{CONFIG_FILE}.broken-")))
        .collect();
    assert_eq!(backups.len(), 1, "恰一份备份：{backups:?}");
    let backup = dir.join(backups.remove(0));
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        original,
        "备份必须与原文逐字节一致"
    );
    assert!(!path.exists(), "原位文件应已被 rename 挪走");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A8 备份失败（目标名被只读目录抢占的重命名失败也包含在内）仍走 C16 的 Err：
/// 唯一可恢复的原文绝不能被向导覆盖
#[test]
fn non_utf8_config_backup_failure_is_fatal() {
    // 构造：锚点目录里先建一个同名 broken 备份文件并 chmod 444，
    // rename 需要写锚点目录本身——把锚点目录置为只读即可让 rename 失败
    let dir = std::env::temp_dir().join(format!("mbx-conf-a8f-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, b"display_name = \"caf\xFF\"\n").unwrap();
    set_readonly(&dir, true);

    let res = super::load(&dir);
    set_readonly(&dir, false); // 无论结果先恢复权限，保证清理可执行
    let err = res.expect_err("备份失败必须 Err（C16），不得静默按未配置启动");
    assert!(err.contains("备份失败"), "错误应说明真实原因：{err}");
    assert!(
        std::fs::read(&path).is_ok_and(|t| t.windows(3).any(|w| w == b"caf")),
        "原文必须原样保留在原位"
    );
    set_readonly(&dir, false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 目录只读开关（unix chmod / windows 目录只读属性不可靠则跳过本测）
fn set_readonly(path: &Path, on: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(if on { 0o555 } else { 0o755 });
        std::fs::set_permissions(path, perm).unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = (path, on);
    }
}

/// R17 P2：配置损坏备份名必须唯一——旧实现用秒级 stamp，同一秒内二次
/// 损坏会静默覆盖上一份备份（可能含唯一有效的 data_dir 指针）。
#[test]
fn r17_corrupt_backups_never_overwrite_each_other() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-r17-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for n in 1..=2 {
        std::fs::write(dir.join(CONFIG_FILE), format!("bad toml #{n} ===")).unwrap();
        assert!(load(&dir).is_empty(), "第 {n} 次损坏应降级为未配置");
    }
    let backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("corpmail.toml.broken-")
        })
        .collect();
    assert_eq!(backups.len(), 2, "两次损坏须留两份备份（旧实现秒级同名覆盖只剩 1 份）");
    let mut texts: Vec<_> = backups
        .iter()
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    texts.sort();
    assert_eq!(texts, vec!["bad toml #1 ===", "bad toml #2 ==="], "两份原文都在");
    let _ = std::fs::remove_dir_all(&dir);
}

/// R19：候选名循环的末位也必须检查。旧实现只检查 base..base-(n-2)，
/// 最后一个候选未经 exists() 就 rename，极端碰撞会覆盖已有备份。
#[test]
fn r19_last_backup_candidate_is_checked() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-r19-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let original = "data_dir_override = \"保留我\"\n坏 toml ===";
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, original).unwrap();
    let stamp = (1_700_000_001u64, 987_654_321u32);
    let base = format!(
        "{CONFIG_FILE}.broken-{}-{:09}-{}",
        stamp.0,
        stamp.1,
        std::process::id()
    );
    std::fs::create_dir_all(dir.join(&base)).unwrap();
    std::fs::create_dir_all(dir.join(format!("{base}-1"))).unwrap();

    let err = broken_fallback_at(&dir, &path, "测试末位候选", stamp, 2)
        .expect_err("所有候选被占用时必须报错，不能覆盖已有备份");
    assert!(err.contains("均被占用"), "错误须说明候选耗尽：{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "原文必须原样保留"
    );
    assert!(
        dir.join(&base).is_dir(),
        "第一个占位目录不得被替换"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R22 P3：同秒同纳秒同 PID 的第 10+ 份备份，字典序 `-10 < -2` 会把更新的
/// 备份裁掉。按「前缀 + 数值后缀」排序后必须保留数值最大的三份。
#[test]
fn prune_broken_backups_orders_suffix_numerically() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let base = format!(
        "{CONFIG_FILE}.broken-1700000000-000000001-{}",
        std::process::id()
    );
    let mut names = vec![base.clone()];
    for i in 1..12 {
        names.push(format!("{base}-{i}"));
    }
    for n in &names {
        std::fs::write(dir.join(n), b"backup").unwrap();
    }
    prune_broken_backups(&dir, 3);
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec![format!("{base}-10"), format!("{base}-11"), format!("{base}-9")],
        "须按数值后缀保留最新三份（字典序排列后的期望）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
