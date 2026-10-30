//! 凭据保管（设计 §5.4）：OS keychain 优先，keyfile 加密回退
//!
//! - **主路径** keyring 4：Windows Credential Manager（generic credential，
//!   service="corpmail"，user=账号）/ macOS Keychain
//! - **回退** keyring 写失败（权限/组策略）→ keyfile：AES-256-GCM 加密落库
//!   （settings 表 `enc:<account>` 行），密钥单独存**库外文件** `corpmail.key`
//!   （锚点目录，0600），设置页提示降级状态
//! - 密码仅运行时读入内存；UI 永不回显明文（输入框留空 = 不修改）

use crate::db;
use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
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
///
/// `key_file`：回退密钥文件路径（锚点目录）。只有走回退分支才会被创建。
pub fn store(
    conn: &Connection,
    key_file: &Path,
    account: &str,
    password: &str,
) -> Result<bool, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())?;
    match entry.set_password(password) {
        Ok(()) => {
            cache_put(account, GetResult::Keychain(password.to_string()));
            Ok(false)
        }
        Err(_) => {
            // 回退：AES-256-GCM 落库，密钥在库外文件（设计 §5.4）
            keyfile_store(conn, key_file, account, password).map_err(|e| e.to_string())?;
            cache_put(account, GetResult::Keyfile(password.to_string()));
            Ok(true)
        }
    }
}

/// 读取密码：先查进程内缓存，未命中再试 keychain，keychain 不可用/无条目时
/// 落到 keyfile 回退（平台后端异常与 NoEntry 同处处理——两者都只剩 keyfile 一条路）。
/// Missing 不入缓存：用户可能随后在设置页补录，缓存空结果会让补录白做。
pub fn retrieve(conn: &Connection, key_file: &Path, account: &str) -> GetResult {
    if let Some(hit) = cache_get(account) {
        return hit;
    }
    let out = match keyring::Entry::new(KEYRING_SERVICE, account) {
        Ok(entry) => match entry.get_password() {
            Ok(p) => GetResult::Keychain(p),
            Err(_) => keyfile_retrieve(conn, key_file, account),
        },
        Err(_) => keyfile_retrieve(conn, key_file, account),
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
pub fn degraded(conn: &Connection, key_file: &Path, account: &str) -> bool {
    matches!(retrieve(conn, key_file, account), GetResult::Keyfile(_))
}

// ---------- keyfile 回退实现（AES-256-GCM + 库外密钥文件） ----------

/// 回退密钥文件名（位于锚点目录，即 corpmail.toml 同处）。
///
/// **必须与数据库分开存放**：历史实现把密钥写在 settings 表的 `enc:keyfile` 行，
/// 与密文同库——拿到 corpmail.db 的人可直接解出全部密码，加密等同于明文。
/// 分离后单靠库文件（备份、同步盘、误传）无法解密。锚点目录不随数据目录迁移，
/// 因此数据目录放到外置盘/U 盘也不会把密钥一起带走。
pub const KEY_FILE: &str = "corpmail.key";

/// 历史版本存放密钥的 settings 行（迁移用，见 `keyfile_key`）
const LEGACY_KEY_ROW: &str = "enc:keyfile";

fn decode_key(text: &[u8]) -> Option<[u8; 32]> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    if raw.len() != 32 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw);
    Some(key)
}

/// 读取密钥文件：`Ok(None)` = 文件不存在；`Err` = 存在但不可用。
///
/// 必须区分这两种情况：文件损坏时若当作"不存在"去重新生成，会把唯一能解开
/// 现有密文的密钥覆盖掉，等于静默销毁全部已存密码。
fn read_key_file(path: &Path) -> Result<Option<[u8; 32]>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => match decode_key(text.trim().as_bytes()) {
            Some(key) => Ok(Some(key)),
            None => Err(format!(
                "回退密钥文件格式异常（{}），已拒绝覆盖以免现有密码永久不可解",
                path.display()
            )),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("读取回退密钥失败（{}）：{err}", path.display())),
    }
}

/// 紧凑权限（unix 0600）：密钥文件只允许属主读写。
/// Windows 侧依赖 %APPDATA% 目录的默认用户级 ACL，不做额外设置。
#[cfg(unix)]
fn harden_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn harden_perms(_path: &Path) {}

/// 原子写密钥文件（0600）：先写同目录临时文件再 rename。
/// 直接覆盖原文件时若进程中途被杀，会留下半截密钥——那等于现有密文永久不可解。
fn write_key_file(path: &Path, key: &[u8; 32]) -> Result<(), String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| KEY_FILE.to_string());
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = opts
            .open(&tmp)
            .map_err(|e| format!("创建密钥文件失败（{}）：{e}", tmp.display()))?;
        file.write_all(b64.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("写入密钥文件失败（{}）：{e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("落定密钥文件失败（{}）：{e}", path.display())
    })?;
    harden_perms(path);
    Ok(())
}

/// 密钥文件创建锁。
///
/// 首次走回退路径时可能有两个调用方同时到达（UI 保存密码 + 收件线程读密码）：
/// 各自生成一把 key 时，先写的密文会被后一次 rename 覆盖成另一把 key，
/// 该密码从此解不开。锁只覆盖本地文件读写，不含任何用户交互，不会长等。
fn key_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 取回退密钥。
///
/// 顺序：密钥文件 → 历史库内密钥（一次性迁出后删行）→ 生成新密钥（仅 `ensure`）。
/// 迁移只在密钥落盘成功后才删库内行：写盘失败要保持库内副本，否则该机器上
/// 所有已存密码会随一次磁盘异常一起消失。
fn keyfile_key(conn: &Connection, key_file: &Path, ensure: bool) -> Result<[u8; 32], String> {
    let _guard = key_lock().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(key) = read_key_file(key_file)? {
        return Ok(key);
    }
    // 历史迁移：旧版本把密钥与密文放同一个库
    if let Some(key) = db::settings::get(conn, LEGACY_KEY_ROW)
        .map_err(|e| e.to_string())?
        .and_then(|b64| decode_key(b64.trim().as_bytes()))
    {
        write_key_file(key_file, &key)?;
        let _ = conn.execute("DELETE FROM settings WHERE k = ?1", [LEGACY_KEY_ROW]);
        return Ok(key);
    }
    if !ensure {
        return Err("回退密钥不存在".into());
    }
    // 生成新 key（熵来自 OS RNG）；**只写文件，绝不落库**
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    write_key_file(key_file, &key)?;
    Ok(key)
}

fn keyfile_store(
    conn: &Connection,
    key_file: &Path,
    account: &str,
    password: &str,
) -> Result<(), String> {
    let key = keyfile_key(conn, key_file, true)?;
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

fn keyfile_retrieve(conn: &Connection, key_file: &Path, account: &str) -> GetResult {
    let key = match keyfile_key(conn, key_file, false) {
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
