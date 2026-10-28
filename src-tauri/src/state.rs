//! 应用状态：SQLite 单连接（`Mutex` 内部可变性）+ 业务配置读写
//!
//! 设计 §2/§13-R6：全库走单一 `Mutex<Connection>`，WAL 模式读写不互阻。
//! 账号密码不落库（经 `secrets` 走 OS keychain，keyfile 回退，设计 §5.4）。

use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// settings 键名（设计 §3；新增键须同步设计文档）
pub mod keys {
    pub const ACCOUNT_PREFIX: &str = "account_prefix";
    pub const ACCOUNT_DOMAIN: &str = "account_domain";
    pub const DISPLAY_NAME: &str = "display_name";
    pub const SMTP_HOST: &str = "smtp_host";
    pub const SMTP_PORT: &str = "smtp_port";
    pub const RECV_HOST: &str = "recv_host";
    pub const RECV_PORT: &str = "recv_port";
    pub const SYNC_ON_START: &str = "sync_on_start";
    pub const SYNC_INTERVAL_MIN: &str = "sync_interval_min";
    pub const FIRST_FETCH_DAYS: &str = "first_fetch_days";
    pub const INLINE_ATTACH_MAX_MB: &str = "inline_attach_max_mb";
    pub const CLEAN_SENT_BULK_DAYS: &str = "clean_sent_bulk_days";
    pub const KEEP_INBOX_DAYS: &str = "keep_inbox_days";
    pub const KEEP_SENT_DAYS: &str = "keep_sent_days";
}

/// 账号域名默认值（设计确认项 K 原为固定后缀；自 v0.1.1 起作为可配置设置项）
pub const ACCOUNT_DOMAIN_DEFAULT: &str = "@chinatelecom.cn";

/// 预填默认值（设计 §8；向导中可改）
pub const SMTP_HOST_DEFAULT: &str = "smtp.chinatelecom.cn";
pub const SMTP_PORT_DEFAULT: &str = "465";
pub const RECV_HOST_DEFAULT: &str = "imap.chinatelecom.cn";
pub const RECV_PORT_DEFAULT: &str = "993";

/// 全部 settings 键的默认值（未配置时表单回显用）
pub fn defaults() -> Vec<(&'static str, &'static str)> {
    vec![
        (keys::ACCOUNT_DOMAIN, ACCOUNT_DOMAIN_DEFAULT),
        (keys::SMTP_HOST, SMTP_HOST_DEFAULT),
        (keys::SMTP_PORT, SMTP_PORT_DEFAULT),
        (keys::RECV_HOST, RECV_HOST_DEFAULT),
        (keys::RECV_PORT, RECV_PORT_DEFAULT),
        (keys::SYNC_ON_START, "true"),
        (keys::SYNC_INTERVAL_MIN, "30"),
        (keys::FIRST_FETCH_DAYS, "30"),
        (keys::INLINE_ATTACH_MAX_MB, "20"),
        (keys::CLEAN_SENT_BULK_DAYS, "7"),
        (keys::KEEP_INBOX_DAYS, "365"),
        (keys::KEEP_SENT_DAYS, "180"),
    ]
}

#[derive(Debug)]
pub struct AppState {
    db: Mutex<Connection>,
    /// 应用数据目录（库/附件根；设置·存储页展示）
    pub data_dir: PathBuf,
}

/// 拼接完整邮箱地址：前缀 + 域名（§8；域名可配置，缺前导 @ 自动补齐）
pub fn full_address(prefix: &str, domain: &str) -> String {
    let d = domain.trim().trim_start_matches('@');
    if d.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}@{d}")
    }
}

impl AppState {
    /// 打开/初始化数据库（schema 幂等迁移在 db::open 内完成）
    pub fn open(data_dir: &Path) -> Self {
        std::fs::create_dir_all(data_dir).expect("数据目录应可创建");
        let conn = crate::db::open(&data_dir.join("mailbox.db")).expect("数据库应可打开");
        Self {
            db: Mutex::new(conn),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// 是否已完成首启配置（向导判定依据：账号前缀已写入）
    pub fn configured(&self) -> bool {
        self.get(keys::ACCOUNT_PREFIX)
            .map(|v| v.filter(|s| !s.is_empty()).is_some())
            .unwrap_or(false)
    }

    /// 读 settings 键（None = 未设置；库错误按未设置处理，页面回退默认值）
    pub fn get(&self, key: &str) -> Option<Option<String>> {
        let conn = self.db.lock().expect("数据库锁中毒");
        crate::db::settings::get(&conn, key).ok()
    }

    /// 读键，未设置回退默认值表
    pub fn get_or(&self, key: &str, defaults: &[(&'static str, &'static str)]) -> Option<String> {
        if let Some(Some(v)) = self.get(key) {
            return Some(v);
        }
        defaults
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_string())
    }

    /// 批量写（单事务；单键写入同样走此接口，保持全库单一写入口）
    pub fn set_batch(&self, pairs: &[(&str, &str)]) -> Result<(), rusqlite::Error> {
        let mut conn = self.db.lock().expect("数据库锁中毒");
        let tx = conn.transaction()?;
        for (k, v) in pairs {
            tx.execute(
                "INSERT INTO settings(k, v) VALUES (?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                [k, v],
            )?;
        }
        tx.commit()
    }

    /// 数据库文件大小（字节；设置·存储页"邮箱数据库大小"）
    pub fn db_size_bytes(&self) -> u64 {
        std::fs::metadata(self.data_dir.join("mailbox.db"))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// SQLite 版本串（关于页展示）
    pub fn sqlite_version(&self) -> String {
        let conn = self.db.lock().expect("数据库锁中毒");
        conn.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))
            .unwrap_or_default()
    }

    /// 侧栏文件夹计数（列表屏冗余展示；M1 无邮件恒为 0，查询面向 M2 复用）
    pub fn folder_counts(&self) -> HashMap<&'static str, i64> {
        let conn = self.db.lock().expect("数据库锁中毒");
        let one = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        let mut m = HashMap::new();
        m.insert(
            "inbox",
            one("SELECT count(*) FROM emails WHERE folder = 'inbox'"),
        );
        m.insert(
            "inbox_unread",
            one("SELECT count(*) FROM emails WHERE folder = 'inbox' AND unread = 1"),
        );
        m.insert("all", one("SELECT count(*) FROM emails"));
        m.insert(
            "sent",
            one("SELECT count(*) FROM emails WHERE folder = 'sent'"),
        );
        m.insert(
            "draft",
            one("SELECT count(*) FROM emails WHERE folder = 'draft'"),
        );
        m
    }

    /// 存密码（secrets 封装；返回是否发生 keyfile 降级）
    pub fn store_password(&self, account: &str, password: &str) -> Result<bool, String> {
        let conn = self.db.lock().expect("数据库锁中毒");
        crate::secrets::store(&conn, account, password)
    }

    /// 读密码（keychain 优先，keyfile 回退）
    pub fn retrieve_password(&self, account: &str) -> crate::secrets::GetResult {
        let conn = self.db.lock().expect("数据库锁中毒");
        crate::secrets::retrieve(&conn, account)
    }

    /// 删除密码（账号变更迁移时用；两条路径都清，幂等）
    pub fn clear_password(&self, account: &str) {
        let conn = self.db.lock().expect("数据库锁中毒");
        crate::secrets::clear(&conn, account);
    }

    /// keychain 是否处于 keyfile 降级态（设置·账号提示用）
    pub fn secrets_degraded(&self, account: &str) -> bool {
        let conn = self.db.lock().expect("数据库锁中毒");
        crate::secrets::degraded(&conn, account)
    }

    /// 数据目录下的独立附件占用（M1 尚无附件；目录不存在 → 0）：(字节, 文件数)
    pub fn attachments_stats(&self) -> (u64, u64) {
        fn dir_stats(p: &Path) -> (u64, u64) {
            let mut bytes = 0;
            let mut files = 0;
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    match e.metadata() {
                        Ok(m) if m.is_file() => {
                            bytes += m.len();
                            files += 1;
                        }
                        Ok(m) if m.is_dir() => {
                            let (b, f) = dir_stats(&e.path());
                            bytes += b;
                            files += f;
                        }
                        _ => {}
                    }
                }
            }
            (bytes, files)
        }
        dir_stats(&self.data_dir.join("attachments"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mbx-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_state_is_unconfigured() {
        let dir = temp_dir("fresh");
        let s = AppState::open(&dir);
        assert!(!s.configured(), "全新库应未配置");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configured_after_prefix_set() {
        let dir = temp_dir("cfg");
        let s = AppState::open(&dir);
        s.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
        assert!(s.configured());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_write_reads_back() {
        let dir = temp_dir("batch");
        let s = AppState::open(&dir);
        s.set_batch(&[
            (keys::SMTP_HOST, "smtp.example.cn"),
            (keys::RECV_HOST, "imap.example.cn"),
        ])
        .unwrap();
        assert_eq!(
            s.get(keys::SMTP_HOST).unwrap().as_deref(),
            Some("smtp.example.cn")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_or_falls_back_to_default() {
        let dir = temp_dir("def");
        let s = AppState::open(&dir);
        let d = defaults();
        assert_eq!(
            s.get_or(keys::SMTP_HOST, &d).as_deref(),
            Some(SMTP_HOST_DEFAULT),
            "未设置时回退预填默认值"
        );
        s.set_batch(&[(keys::SMTP_HOST, "smtp.other.cn")]).unwrap();
        assert_eq!(
            s.get_or(keys::SMTP_HOST, &d).as_deref(),
            Some("smtp.other.cn")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_counts_start_at_zero() {
        let dir = temp_dir("counts");
        let s = AppState::open(&dir);
        let c = s.folder_counts();
        assert_eq!(c["inbox"], 0);
        assert_eq!(c["inbox_unread"], 0);
        assert_eq!(c["all"], 0);
        assert_eq!(c["sent"], 0);
        assert_eq!(c["draft"], 0);
        assert_eq!(s.attachments_stats(), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_address_appends_domain() {
        assert_eq!(
            full_address("demo", "@chinatelecom.cn"),
            "demo@chinatelecom.cn",
            "默认域名拼接"
        );
        assert_eq!(
            full_address("demo", "chinatelecom.cn"),
            "demo@chinatelecom.cn",
            "缺前导 @ 自动补齐"
        );
        assert_eq!(full_address("demo", ""), "demo", "空域名退化为前缀");
    }
}
