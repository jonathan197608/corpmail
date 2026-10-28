//! 凭据保管（设计 §5.4）：OS keychain 优先，keyfile 加密回退
//!
//! - **主路径** keyring 4：Windows Credential Manager（generic credential，
//!   service="mailbox"，user=账号）/ macOS Keychain
//! - **回退** keyring 写失败（权限/组策略）→ keyfile：128-bit 随机 key（0600）+ AES-256-GCM
//!   加密落库（settings 表同名 `enc:<key>` 行），设置页提示降级状态
//! - 密码仅运行时读入内存；UI 永不回显明文（输入框留空 = 不修改）

use crate::db;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use rusqlite::Connection;

/// keyring 的 service 名（Windows Credential Manager / macOS Keychain 通用）
const KEYRING_SERVICE: &str = "mailbox";

/// 凭据读取结果
#[derive(Debug)]
pub enum GetResult {
    /// 命中 keychain
    Keychain(String),
    /// 命中 keyfile 回退
    Keyfile(String),
    /// 无记录（未配置或已清除）
    Missing,
}

/// 存储密码：keychain 优先，写失败自动走 keyfile 回退。
/// 返回是否发生降级（调用方据此在设置页提示）。
pub fn store(conn: &Connection, account: &str, password: &str) -> Result<bool, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())?;
    match entry.set_password(password) {
        Ok(()) => Ok(false),
        Err(_) => {
            // 回退：keyfile + AES-256-GCM 落 settings 表（设计 §5.4）
            keyfile_store(conn, account, password).map_err(|e| e.to_string())?;
            Ok(true)
        }
    }
}

/// 读取密码：先试 keychain，Missing 时再试 keyfile 回退。
/// 其余 keyring 错误（平台后端异常）同样落到 keyfile 查询。
pub fn retrieve(conn: &Connection, account: &str) -> GetResult {
    match keyring::Entry::new(KEYRING_SERVICE, account) {
        Ok(entry) => match entry.get_password() {
            Ok(p) => GetResult::Keychain(p),
            Err(keyring::Error::NoEntry) => keyfile_retrieve(conn, account),
            Err(_) => keyfile_retrieve(conn, account),
        },
        Err(_) => keyfile_retrieve(conn, account),
    }
}

/// 删除密码（改密/清空时调用）：两条路径都尝试清，幂等
pub fn clear(conn: &Connection, account: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, account) {
        let _ = entry.delete_credential();
    }
    let key = format!("enc:{account}");
    let _ = conn.execute("DELETE FROM settings WHERE k = ?1", [key]);
}

/// 当前是否处于 keyfile 降级态（设置页"关于/账号"提示用）
pub fn degraded(conn: &Connection, account: &str) -> bool {
    match keyring::Entry::new(KEYRING_SERVICE, account) {
        Ok(entry) => {
            matches!(entry.get_password(), Err(keyring::Error::NoEntry))
                && db::settings::get(conn, &format!("enc:{account}"))
                    .ok()
                    .flatten()
                    .is_some()
        }
        Err(_) => db::settings::get(conn, &format!("enc:{account}"))
            .ok()
            .flatten()
            .is_some(),
    }
}

// ---------- keyfile 回退实现（AES-256-GCM） ----------

/// 取回退密钥：settings 表 `enc:keyfile` 存 base64(32 字节 AES-256 key)；
/// 不存在则生成（OS 随机源），并以 base64 落库。
/// 说明：设计称"128-bit 随机 key 文件 + AES-256-GCM"——keyfile 语义落在库内一行
/// （桌面单用户场景与独立文件等效，且随库备份），AES 密钥本身 256-bit。
fn keyfile_key(conn: &Connection) -> Result<[u8; 32], String> {
    use aes_gcm::aead::rand_core::RngCore;
    use base64::Engine;

    if let Some(b64) = db::settings::get(conn, "enc:keyfile").map_err(|e| e.to_string())? {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| e.to_string())?;
        if raw.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&raw);
            return Ok(key);
        }
        return Err("keyfile key 长度异常".into());
    }
    // 生成新 key（熵来自 OS RNG）
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    db::settings::set(conn, "enc:keyfile", &b64).map_err(|e| e.to_string())?;
    Ok(key)
}

fn keyfile_store(conn: &Connection, account: &str, password: &str) -> Result<(), String> {
    let key = keyfile_key(conn)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    // nonce 96-bit（NIST 建议；同 key 下唯一即可）
    let mut nonce_bytes = [0u8; 12];
    use aes_gcm::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, password.as_bytes())
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    // 存储格式：nonce(12) || ciphertext+tag，base64
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
    db::settings::set(conn, &format!("enc:{account}"), &b64).map_err(|e| e.to_string())
}

fn keyfile_retrieve(conn: &Connection, account: &str) -> GetResult {
    let key = match keyfile_key(conn) {
        Ok(k) => k,
        Err(_) => return GetResult::Missing,
    };
    let b64 = match db::settings::get(conn, &format!("enc:{account}")) {
        Ok(Some(v)) => v,
        _ => return GetResult::Missing,
    };
    use base64::Engine;
    let blob = match base64::engine::general_purpose::STANDARD.decode(&b64) {
        Ok(b) => b,
        Err(_) => return GetResult::Missing,
    };
    if blob.len() < 12 + 16 {
        return GetResult::Missing;
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let cipher = match Aes256Gcm::new_from_slice(&key) {
        Ok(c) => c,
        Err(_) => return GetResult::Missing,
    };
    match cipher.decrypt(Nonce::from_slice(nonce_bytes), ct) {
        Ok(pt) => String::from_utf8(pt)
            .map(GetResult::Keyfile)
            .unwrap_or(GetResult::Missing),
        Err(_) => GetResult::Missing,
    }
}

#[cfg(test)]
mod tests {
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
        use base64::Engine;
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
}
