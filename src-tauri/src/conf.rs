//! 独立应用配置文件（`mailbox.toml`，M5 验收架构）
//!
//! 内容：数据目录指针 + 邮件服务器 + 账号配置（**不含密码**：密码仅 OS keychain /
//! keyfile，设计 §5.4）。位置：锚点目录（系统默认应用数据目录），与数据目录解耦——
//! 数据迁移、删除旧库备份均不触碰本文件。
//!
//! 唯一持久源（验收拍板）：读写仅经本文件，不做任何 DB 镜像或启动回填；
//! 文件缺失/为空即未配置状态（重新走向导）。

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

/// 读取全部配置（文件缺失/损坏 → 空 map，即未配置状态；未知键过滤——
/// 配置文件内仅承载 CONFIG_KEYS）
pub fn load(anchor: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(anchor.join(CONFIG_FILE))
        .ok()
        .and_then(|t| toml::from_str::<BTreeMap<String, String>>(&t).ok())
        .unwrap_or_default()
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
    if let Err(e) = std::fs::rename(&tmp, &path) {
        // rename 失败（目标被占用等）：清掉临时残片，下次重试从干净状态开始
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("配置落盘失败：{e}"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/conf.rs"]
mod tests;
