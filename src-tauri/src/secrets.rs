//! 凭据保管（设计 §5.4）：OS keychain 优先，keyfile 加密回退
//!
//! - **主路径** keyring 4：Windows Credential Manager（generic credential，
//!   service="corpmail"，user=账号）/ macOS Keychain
//! - **回退** keyring 写失败（权限/组策略）→ keyfile：128-bit 随机 key（0600）+ AES-256-GCM
//!   加密落库（settings 表同名 `enc:<key>` 行），设置页提示降级状态
//! - 密码仅运行时读入内存；UI 永不回显明文（输入框留空 = 不修改）

use crate::db;
use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// keyring 的 service 名（Windows Credential Manager / macOS Keychain 通用）。
/// 品牌统一后旧服务名 "mailbox" 下的凭据不迁移（0.2.x 无存量用户）
const KEYRING_SERVICE: &str = "corpmail";

/// 凭据读取结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetResult {
    /// 命中 keychain
    Keychain(String),
    /// 命中 keyfile 回退
    Keyfile(String),
    /// 无记录（未配置或已清除）
    Missing,
}

// ---------- 进程内密码缓存 ----------
//
// macOS 每次读 keychain 都要校验调用方代码签名：条目 ACL 记录的是创建时进程的
// designated requirement。本项目为 ad-hoc 签名（TeamIdentifier=not set，DR 即
// cdhash），每次重新链接 cdhash 都变 → ACL 必然失配 → 反复弹「想要使用您存储在
// 钥匙串中的机密信息」。用户点「始终允许」也只对当次 cdhash 生效，下次构建复现。
//
// 因此把已读到的密码缓存在进程内：同一账号一次运行最多访问 keychain 一次。
// 不做 TTL——进程内密码由 store/clear 主动失效，改密路径已覆盖；跨进程变更
// （别的客户端改密）会导致发送认证失败，用户改密后走 store 自然刷新。
//
// 锁只保护 map，绝不跨 keychain 调用持有：授权对话框可能要等用户点几秒到无限久，
// 持锁会把 UI 线程与收件/发信线程一起卡死。
type PasswordCache = Mutex<HashMap<String, GetResult>>;

fn cache() -> &'static PasswordCache {
    static CACHE: OnceLock<PasswordCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(account: &str) -> Option<GetResult> {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(account)
        .cloned()
}

/// 只查缓存、绝不触碰 keychain 或数据库。
/// 供 `AppState::retrieve_password` 在锁 db 之前短路：keychain 授权窗口可能
/// 等用户点几秒到无限久，命中缓存就不该把整个 DB 锁一起搭进去。
pub(crate) fn cached(account: &str) -> Option<GetResult> {
    cache_get(account)
}

fn cache_put(account: &str, value: GetResult) {
    let _ = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(account.to_string(), value);
}

fn cache_drop(account: &str) {
    let _ = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(account);
}

/// 存储密码：keychain 优先，写失败自动走 keyfile 回退。
/// 返回是否发生降级（调用方据此在设置页提示）。成功后立即刷新缓存，
/// 保证本进程后续读取拿到新密码而不是旧缓存。
pub fn store(conn: &Connection, account: &str, password: &str) -> Result<bool, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())?;
    match entry.set_password(password) {
        Ok(()) => {
            cache_put(account, GetResult::Keychain(password.to_string()));
            Ok(false)
        }
        Err(_) => {
            // 回退：keyfile + AES-256-GCM 落 settings 表（设计 §5.4）
            keyfile_store(conn, account, password).map_err(|e| e.to_string())?;
            cache_put(account, GetResult::Keyfile(password.to_string()));
            Ok(true)
        }
    }
}

/// 读取密码：先查进程内缓存，未命中再试 keychain，keychain 不可用/无条目时
/// 落到 keyfile 回退（平台后端异常与 NoEntry 同处处理——两者都只剩 keyfile 一条路）。
/// Missing 不入缓存：用户可能随后在设置页补录，缓存空结果会让补录白做。
pub fn retrieve(conn: &Connection, account: &str) -> GetResult {
    if let Some(hit) = cache_get(account) {
        return hit;
    }
    let out = match keyring::Entry::new(KEYRING_SERVICE, account) {
        Ok(entry) => match entry.get_password() {
            Ok(p) => GetResult::Keychain(p),
            Err(_) => keyfile_retrieve(conn, account),
        },
        Err(_) => keyfile_retrieve(conn, account),
    };
    if !matches!(out, GetResult::Missing) {
        cache_put(account, out.clone());
    }
    out
}

/// 删除密码（改密/清空时调用）：两条路径都尝试清，幂等；同时丢弃缓存，
/// 否则本进程仍会用已删的旧密码继续发信
pub fn clear(conn: &Connection, account: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, account) {
        let _ = entry.delete_credential();
    }
    let key = format!("enc:{account}");
    let _ = conn.execute("DELETE FROM settings WHERE k = ?1", [key]);
    cache_drop(account);
}

/// 当前是否处于 keyfile 降级态（设置页"关于/账号"提示用）。
/// 直接复用 retrieve：缓存命中时不再触碰 keychain（此前每次渲染设置页都调一次
/// get_password，是 macOS 弹窗的第二个来源）。语义上要求 keyfile 记录**可解密**
/// 才算降级——行存在但解不开属数据损坏，不该再报"密码已存本地"
pub fn degraded(conn: &Connection, account: &str) -> bool {
    matches!(retrieve(conn, account), GetResult::Keyfile(_))
}

// ---------- keyfile 回退实现（AES-256-GCM） ----------

/// 取回退密钥：settings 表 `enc:keyfile` 存 base64(32 字节 AES-256 key)；
/// 不存在则生成（OS 随机源），并以 base64 落库。
/// 说明：设计称"128-bit 随机 key 文件 + AES-256-GCM"——keyfile 语义落在库内一行
/// （桌面单用户场景与独立文件等效，且随库备份），AES 密钥本身 256-bit。
fn keyfile_key(conn: &Connection) -> Result<[u8; 32], String> {
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
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, password.as_bytes())
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    // 存储格式：nonce(12) || ciphertext+tag，base64
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
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
#[path = "tests/secrets.rs"]
mod tests;
