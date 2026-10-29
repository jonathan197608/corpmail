//! 独立应用配置文件（`mailbox.toml`，M5 验收架构）
//!
//! 内容：数据目录指针 + 邮件服务器 + 账号配置（**不含密码**：密码仅 OS keychain /
//! keyfile，设计 §5.4）。位置：锚点目录（系统默认应用数据目录），与数据目录解耦——
//! 数据迁移、删除旧库备份均不触碰本文件。
//!
//! 配置唯一持久源（验收拍板简化）：写入仅落本文件，**不镜像到 DB**；
//! 启动时若文件缺失键，从旧版 DB settings 回填一次（0.1.0-0.2.2 升级路径）。

use std::collections::BTreeMap;
use std::path::Path;

/// 配置键值表（BTreeMap：文件与内存镜像同构，序列化按键排序稳定）
pub type ConfigMap = BTreeMap<String, String>;

/// 配置文件名（锚点目录下）
pub const CONFIG_FILE: &str = "mailbox.toml";

/// 配置文件承载键（与 DB settings 同名，读写路由见 AppState::get/set_batch）
pub const CONFIG_KEYS: &[&str] = &[
    crate::state::keys::DATA_DIR,
    crate::state::keys::ACCOUNT_PREFIX,
    crate::state::keys::ACCOUNT_DOMAIN,
    crate::state::keys::DISPLAY_NAME,
    crate::state::keys::SMTP_HOST,
    crate::state::keys::SMTP_PORT,
    crate::state::keys::RECV_HOST,
    crate::state::keys::RECV_PORT,
];

/// 键是否由配置文件承载
pub fn is_config_key(key: &str) -> bool {
    CONFIG_KEYS.contains(&key)
}

/// 读取全部配置（文件缺失/损坏 → 空 map，由 DB 镜像自愈；未知键过滤——
/// 配置文件内仅承载 CONFIG_KEYS）
pub fn load(anchor: &Path) -> BTreeMap<String, String> {
    let parsed: BTreeMap<String, String> = std::fs::read_to_string(anchor.join(CONFIG_FILE))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default();
    parsed
        .into_iter()
        .filter(|(k, _)| is_config_key(k))
        .collect()
}

/// 原子保存（临时文件 + rename：防写一半崩溃留残片）。
/// BTreeMap 序列化为排序平铺键值；文件头附说明注释。
pub fn save(anchor: &Path, cfg: &BTreeMap<String, String>) -> Result<(), String> {
    let mut text = String::from(
        "# Mailbox 应用配置（独立于数据目录：迁移数据、删除旧库备份均不影响本文件）\n\
         # data_dir_override 为数据目录指针，留空表示使用默认目录；请勿删除本文件\n",
    );
    let body = toml::to_string(cfg).map_err(|e| format!("配置序列化失败：{e}"))?;
    text.push_str(&body);
    std::fs::create_dir_all(anchor).map_err(|e| format!("配置目录不可创建：{e}"))?;
    let path = anchor.join(CONFIG_FILE);
    let tmp = anchor.join(format!("{CONFIG_FILE}.tmp"));
    std::fs::write(&tmp, text).map_err(|e| format!("配置写入失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("配置落盘失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
}
