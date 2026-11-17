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

/// 测试二进制短路 keychain：真实 keychain 调用在无 GUI 授权环境（CI/后台）
/// 会**挂起等待授权**而非返回错误——降级分支永远等不到，测试进程卡死。
/// tests/secrets.rs 早已绕开 keyring 只测 keyfile 分支，但走 store/retrieve
/// 的向导/发信/dump 测试仍会碰真实 keychain（本机授权过才碰巧能跑）。
/// 默认下统一直走 keyfile，消除对系统钥匙串的隐式依赖。
///
/// 环境变量 `CORPMAIL_KEYCHAIN=real` 可显式恢复真实 keychain（审计 R74：
/// 以前是编译期常量，连手工验证「真 keychain 路径」都没有开关）。只认这一个
/// 取值，其余（含空串/拼错）一律按默认短路处理——避免"看起来打开了其实没开"
/// 生产构建：从不短路 keychain（环境变量只对测试构建有意义）
#[cfg(not(test))]
fn skip_keychain_for(_env: Option<&str>) -> bool {
    false
}

/// 测试构建：默认短路，`CORPMAIL_KEYCHAIN=real` 时走真实 keychain
#[cfg(test)]
fn skip_keychain_for(env: Option<&str>) -> bool {
    !matches!(env, Some("real"))
}

fn skip_keychain() -> bool {
    static SKIP: OnceLock<bool> = OnceLock::new();
    *SKIP.get_or_init(|| skip_keychain_for(std::env::var("CORPMAIL_KEYCHAIN").ok().as_deref()))
}

// ---------- keychain 三操作收口（A1，audit 2026-11-16）----------
//
// store/retrieve/keychain_get 里的 keyring 调用统一抽到这里。目的：
// 1. A1 的「set 失败 → 删旧 keychain 条目 → keyfile 回退」链路必须在单测中被
//    **真实执行生产代码**——但 keyring::Entry 无公开构造器、无法造实例，
//    不抽收口就只能测镜像副本，变异验证（V1）无鉴别力；
// 2. 测试经 FakeKeyring（下方 cfg(test) 模块）替换三个操作，不碰真实钥匙串、
//    不改 skip_keychain()（R74 开关语义与 tests 既有断言全部保留）。

/// keychain 是否参与本轮 store/retrieve。生产恒 true（skip_keychain 的非测试
/// 分支恒 false）；测试默认短路，但 FakeKeyring 激活时强制走 keychain 链路
fn keychain_active() -> bool {
    #[cfg(test)]
    {
        if fake_keyring::active() {
            return true;
        }
    }
    !skip_keychain()
}

fn keychain_set_password(account: &str, password: &str) -> Result<(), String> {
    #[cfg(test)]
    if fake_keyring::active() {
        return fake_keyring::set_password(account, password);
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())?;
    entry.set_password(password).map_err(|e| e.to_string())
}

/// 判定 keychain 平台后端是否**根本不存在**（vs 存在但被锁/ACL 拒绝）。
///
/// macOS 无默认 keychain 时报 "...a default keychain could not be found..."，
/// 这种环境里 keychain 中不可能存有旧密码——删不掉也应放行 keyfile 回退，
/// 否则 e2e 隔离 HOME 的首次保存会被挡死（复查 R1 实测回归）。
/// 独立成纯函数 + 单测钉死：字符串匹配只针对这个特定系统错误，锁定（
/// NoStorageAccess）/ ACL 拒绝的文本不含这些短语，一律走 Err 上抛。
fn keychain_backend_absent(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("default keychain could not be found")
        || m.contains("no default keychain")
        || m.contains("no default store")
}

/// 删除结果的语义归一（生产与 fake 共用，保证单测覆盖真分支）：
/// `Ok(true)`=删掉旧条目；`Ok(false)`=NoEntry；`Err` 且后端根本不存在→无旧
/// 条目可放行；其它 `Err`（NoStorageAccess 锁定 / ACL 拒绝）→旧条目可能存在，
/// 上抛且不写 enc:（复查 R1）。
fn normalize_delete(result: Result<bool, String>) -> Result<bool, String> {
    match result {
        Ok(b) => Ok(b),
        Err(e) if keychain_backend_absent(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// 删 keychain 条目：Ok(已删条目)；NoEntry / 后端不存在视为已清空（幂等语义）
fn keychain_delete_credential(account: &str) -> Result<bool, String> {
    #[cfg(test)]
    if fake_keyring::active() {
        return normalize_delete(fake_keyring::delete_credential(account));
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())?;
    let raw = match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(e.to_string()),
    };
    normalize_delete(raw)
}

fn keychain_get_password(account: &str) -> Option<String> {
    #[cfg(test)]
    if fake_keyring::active() {
        return fake_keyring::get_password(account);
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, account).ok()?;
    entry.get_password().ok()
}

/// FakeKeyring：线程本地的假钥匙串（条目表 + 可注入 set/delete 失败）。
/// 测试串行同线程，thread_local 即隔离；RAII Guard 装卸，panic 也能复位
#[cfg(test)]
mod fake_keyring {
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        entries: HashMap<String, String>,
        fail_set: bool,
        fail_delete: bool,
        /// 条目仍在 entries 中，但 get_password 返回 None（钥匙串锁定 / ACL 拒读）。
        /// 用于钉死 store 的回退决策必须看 delete 结果而非 get 可读性（复查 R1）
        fail_get: bool,
        /// delete 报「无默认 keychain」（e2e 隔离 HOME 形态）：后端根本不存在，
        /// 归一化后应放行 keyfile 回退（复查 R1 实测回归）
        delete_backend_absent: bool,
    }

    thread_local! {
        static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
    }

    pub(super) fn active() -> bool {
        FAKE.with(|c| c.borrow().is_some())
    }

    pub(super) fn set_password(account: &str, password: &str) -> Result<(), String> {
        FAKE.with(|c| {
            let mut guard = c.borrow_mut();
            let Some(f) = guard.as_mut() else {
                return Ok(());
            };
            if f.fail_set {
                return Err("注入的 keychain 写失败".into());
            }
            f.entries.insert(account.to_string(), password.to_string());
            Ok(())
        })
    }

    pub(super) fn delete_credential(account: &str) -> Result<bool, String> {
        FAKE.with(|c| {
            let mut guard = c.borrow_mut();
            let Some(f) = guard.as_mut() else {
                return Ok(false);
            };
            if f.delete_backend_absent {
                // 与 macOS keyring/security-framework 的错误文本一致
                return Err("Platform failure: A default keychain could not be found.".into());
            }
            if f.fail_delete {
                return Err("注入的 keychain 删除失败".into());
            }
            Ok(f.entries.remove(account).is_some())
        })
    }

    pub(super) fn get_password(account: &str) -> Option<String> {
        FAKE.with(|c| {
            c.borrow().as_ref().and_then(|f| {
                if f.fail_get {
                    return None;
                }
                f.entries.get(account).cloned()
            })
        })
    }

    /// RAII 安装/卸载（drop 时复位）。配置方法均为短暂借用，与生产代码
    /// 中收口函数的重入（store → keychain_set_password）不冲突
    pub(super) struct Guard;

    impl Guard {
        pub(super) fn install() -> Self {
            FAKE.with(|c| *c.borrow_mut() = Some(Fake::default()));
            Guard
        }
        pub(super) fn preset(&self, account: &str, password: &str) {
            FAKE.with(|c| {
                if let Some(f) = c.borrow_mut().as_mut() {
                    f.entries.insert(account.to_string(), password.to_string());
                }
            });
        }
        pub(super) fn make_set_fail(&self) {
            FAKE.with(|c| {
                if let Some(f) = c.borrow_mut().as_mut() {
                    f.fail_set = true;
                }
            });
        }
        pub(super) fn make_delete_fail(&self) {
            FAKE.with(|c| {
                if let Some(f) = c.borrow_mut().as_mut() {
                    f.fail_delete = true;
                }
            });
        }
        /// 条目仍在，但 get 返回 None（模拟钥匙串锁定 / ACL 拒读）
        pub(super) fn make_get_fail(&self) {
            FAKE.with(|c| {
                if let Some(f) = c.borrow_mut().as_mut() {
                    f.fail_get = true;
                }
            });
        }
        /// delete 报「无默认 keychain」（e2e 隔离 HOME 形态）
        pub(super) fn make_delete_backend_absent(&self) {
            FAKE.with(|c| {
                if let Some(f) = c.borrow_mut().as_mut() {
                    f.delete_backend_absent = true;
                }
            });
        }
        /// 当前条目（None = 不存在）
        pub(super) fn entry(&self, account: &str) -> Option<String> {
            FAKE.with(|c| {
                c.borrow()
                    .as_ref()
                    .and_then(|f| f.entries.get(account).cloned())
            })
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FAKE.with(|c| *c.borrow_mut() = None);
        }
    }
}

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

impl GetResult {
    /// 取出密码明文；无记录返回 None。
    ///
    /// 收件/发信/已读回写多处都需要"命中即取明文、缺失即跳过"，统一收口，
    /// 避免各调用方重复写 `Keychain(p)|Keyfile(p)=>p` 的 match 样板。
    pub fn into_password(self) -> Option<String> {
        match self {
            GetResult::Keychain(p) | GetResult::Keyfile(p) => Some(p),
            GetResult::Missing => None,
        }
    }
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

#[cfg_attr(not(test), allow(dead_code))]
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
///
/// 测试二进制短路 keychain（见 `skip_keychain`）；FakeKeyring 激活时例外
pub fn store(
    conn: &Connection,
    key_file: &Path,
    account: &str,
    password: &str,
) -> Result<bool, String> {
    if keychain_active() {
        return store_keychain(conn, key_file, account, password);
    }
    keyfile_store(conn, key_file, account, password).map_err(|e| e.to_string())?;
    cache_put(account, GetResult::Keyfile(password.to_string()));
    Ok(true)
}

/// keychain 可用时的 store 主链：写 keychain；写失败走「删旧条目 → keyfile」回退（A1 / 复查 R1）。
///
/// 风险的本质：重启后两段式读取（keychain 优先）读回旧密码 P1、`enc:` 的新密码
/// P2 永不生效，且 `degraded()` 因返回 Keychain 不提示——改密静默失效死循环。
/// 决策依据是**删除结果**而非「旧条目当下是否可读」：
/// - 无条目（`keychain_delete_credential` 把 NoEntry 归一为 `Ok(false)`）→ 无复活
///   路径，回退 keyfile 安全——e2e 隔离 HOME（无默认钥匙串）正是这种形态，必须放行；
/// - 条目存在且删除成功（`Ok(true)`）→ 旧密码已清除，回退安全；
/// - 条目存在但删不掉（`Err`，如钥匙串锁定 / ACL 拒删）→ 返回 `Err` 且**不写 enc:**，
///   绝不制造「看似保存成功、重启后失效」的坏态。
///
/// 不可用 `keychain_get_password(account).is_some()` 判断是否有复活路径：钥匙串
/// 锁定时条目仍在但 get 返回 None，跳过删除会静默双写，与原始 bug 同态（复查 R1）。
fn store_keychain(
    conn: &Connection,
    key_file: &Path,
    account: &str,
    password: &str,
) -> Result<bool, String> {
    match keychain_set_password(account, password) {
        Ok(()) => finish_keychain_store(conn, account, password),
        Err(_) => {
            // 无条件删除旧条目：无条目（NoEntry → Ok(false)）放行回退；删不掉（Err）
            // 上抛且不写 enc:。注意不能依赖 get 的可读性——条目在但当下读不到时
            // 仍必须删，否则重启解锁后读回旧密码（复查 R1）
            if let Err(e) = keychain_delete_credential(account) {
                return Err(format!(
                    "钥匙串中仍存有旧密码且无法清除，本次修改未保存（{e}）。请解锁钥匙串或授权后重试"
                ));
            }
            // 回退：AES-256-GCM 落库，密钥在库外文件（设计 §5.4）
            keyfile_store(conn, key_file, account, password).map_err(|e| e.to_string())?;
            cache_put(account, GetResult::Keyfile(password.to_string()));
            Ok(true)
        }
    }
}

/// keychain 写入成功后的收尾（C17）：清掉 keyfile 回退期留在库内的旧密文。
///
/// 不清会怎样：用户改密后 keychain 一度恢复成功，但 `enc:{account}` 仍是**改密前**
/// 的旧密码；ad-hoc 签名使 keychain ACL 失配是常态，下一次 `retrieve` 回退到
/// keyfile 就会读回旧密码，发信/收件持续认证失败，`degraded()` 也不会提示。
/// `corpmail.key` 保留（其它账号可能还在用），只删对应账号的密文行。
///
/// 独立成函数是为了在「不碰真实 keychain」的前提下钉住这条恢复路径：
/// 测试构建默认短路 keychain，走不到 `store` 的成功分支。
/// 删除失败必须上报：keychain 已写成功但旧密文还在，静默返回 Ok 会留下坏态。
fn finish_keychain_store(conn: &Connection, account: &str, password: &str) -> Result<bool, String> {
    let key = format!("enc:{account}");
    conn.execute("DELETE FROM settings WHERE k = ?1", [key])
        .map(|_| ())
        .map_err(|e| format!("写入 keychain 成功，但清理本地旧密码失败：{e}"))?;
    cache_put(account, GetResult::Keychain(password.to_string()));
    Ok(false)
}

/// 只探 keychain（不碰库、不碰缓存读取；`AppState::retrieve_password` 在取
/// DB 锁之前调用——授权窗口可能等用户几秒到无限久，绝不能带着库锁进来）。
/// 命中返回 Keychain 结果并写入缓存；无条目/后端异常/测试短路返回 None
/// （统一由调用方走 keyfile 回退段）。Missing 语义不属于 keychain 段
pub fn keychain_get(account: &str) -> Option<GetResult> {
    if !keychain_active() {
        return None;
    }
    let password = keychain_get_password(account)?;
    let out = GetResult::Keychain(password);
    cache_put(account, out.clone());
    Some(out)
}

/// keychain 未命中后的库内段：keyfile 回退 + 缓存写入（Missing 不入缓存）。
/// 与 `keychain_get` 配对，供两段式的 `AppState::retrieve_password` 使用
pub fn retrieve_fallback(conn: &Connection, key_file: &Path, account: &str) -> GetResult {
    let out = keyfile_retrieve(conn, key_file, account);
    if !matches!(out, GetResult::Missing) {
        cache_put(account, out.clone());
    }
    out
}

/// 读取密码：先查进程内缓存，未命中再试 keychain，keychain 不可用/无条目时
/// 落到 keyfile 回退（平台后端异常与 NoEntry 同处处理——两者都只剩 keyfile 一条路）。
/// Missing 不入缓存：用户可能随后在设置页补录，缓存空结果会让补录白做。
/// 测试二进制短路 keychain（见 `skip_keychain`）。
/// 取锁路径见 `AppState::retrieve_password`（两段式，keychain 段不持库锁）
pub fn retrieve(conn: &Connection, key_file: &Path, account: &str) -> GetResult {
    if let Some(hit) = cache_get(account) {
        return hit;
    }
    // keychain 段（Entry::new 失败与 get_password 失败同口径：都只剩 keyfile 一条路）
    let out = if keychain_active() {
        match keychain_get_password(account) {
            Some(p) => GetResult::Keychain(p),
            None => keyfile_retrieve(conn, key_file, account),
        }
    } else {
        keyfile_retrieve(conn, key_file, account)
    };
    if !matches!(out, GetResult::Missing) {
        cache_put(account, out.clone());
    }
    out
}

/// 删除密码（改密/清空时调用）：两条路径都尝试清，幂等；同时丢弃缓存，
/// 否则本进程仍会用已删的旧密码继续发信。测试二进制短路 keychain。
///
/// keychain 侧失败可忽略（本来就可能不存在）；库内密文行删除失败必须上报
/// （审计 P1-50）——行残留 + 缓存已丢，下次 retrieve 会读出已"删除"的旧密码
#[cfg_attr(not(test), allow(dead_code))]
pub fn clear(conn: &Connection, account: &str) -> Result<(), String> {
    if !skip_keychain()
        && let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, account)
    {
        let _ = entry.delete_credential();
    }
    let key = format!("enc:{account}");
    let res = conn
        .execute("DELETE FROM settings WHERE k = ?1", [key])
        .map(|_| ())
        .map_err(|e| format!("清除本地密码失败：{e}"));
    cache_drop(account);
    res
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
        // 删库内旧密钥：失败不阻断本次加解密（密钥文件已可用），但必须留痕
        // ——行残留意味着拿到库文件的人仍能解出全部密码（审计 P1-50）
        if let Err(err) = conn.execute("DELETE FROM settings WHERE k = ?1", [LEGACY_KEY_ROW]) {
            eprintln!(
                "[secrets] 迁移后清除库内旧密钥失败（密钥已与密文同库，建议手动清理 {LEGACY_KEY_ROW} 行）：{err}"
            );
        }
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
