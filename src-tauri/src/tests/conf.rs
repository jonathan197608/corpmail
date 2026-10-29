//! conf 模块单元测试（自 src/conf.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;

#[test]
fn save_load_roundtrip_and_corrupt_tolerated() {
    let dir = std::env::temp_dir().join(format!("mbx-conf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = BTreeMap::new();
    cfg.insert(
        crate::state::keys::DATA_DIR.to_string(),
        "/Volumes/data/mail".to_string(),
    );
    cfg.insert(
        crate::state::keys::DISPLAY_NAME.to_string(),
        "张三 \"引号\" = 等号".to_string(),
    );
    save(&dir, &cfg).unwrap();
    let back = load(&dir);
    assert_eq!(
        back.get(crate::state::keys::DATA_DIR).unwrap(),
        "/Volumes/data/mail"
    );
    // 特殊字符 roundtrip 无损
    assert_eq!(
        back.get(crate::state::keys::DISPLAY_NAME).unwrap(),
        "张三 \"引号\" = 等号"
    );
    // 损坏文件 → 空 map（不 panic，自愈路径）
    std::fs::write(dir.join(CONFIG_FILE), "not [ valid toml ===").unwrap();
    assert!(load(&dir).is_empty());
    // 未知键被忽略（只承载 CONFIG_KEYS）
    std::fs::write(
        dir.join(CONFIG_FILE),
        format!(
            "{} = \"x\"\nunknown_key = \"y\"\n",
            crate::state::keys::SMTP_HOST
        ),
    )
    .unwrap();
    let back2 = load(&dir);
    assert_eq!(back2.len(), 1, "未知键应被忽略：{back2:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
