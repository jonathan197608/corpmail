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

    // 预建未来几秒的 broken-* 目录：rename(文件 → 已存在目录) 必失败，
    // 覆盖测试跨秒边界，不依赖文件系统权限
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for d in 0..5 {
        std::fs::create_dir_all(dir.join(format!("{CONFIG_FILE}.broken-{}", now + d))).unwrap();
    }

    let err = super::load(&dir).expect_err("备份失败时必须明确报错，不能静默按未配置启动");
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
