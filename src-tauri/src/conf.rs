//! 独立应用配置文件（`corpmail.toml`，M5 验收架构）
//!
//! 内容：数据目录指针 + 邮件服务器 + 账号配置（**不含密码**：密码仅 OS keychain /
//! keyfile，设计 §5.4）。位置：锚点目录（系统默认应用数据目录），与数据目录解耦——
//! 数据迁移、删除旧库备份均不触碰本文件。
//!
//! 唯一持久源（验收拍板）：读写仅经本文件，不做任何 DB 镜像或启动回填；
//! 文件缺失/为空即未配置状态（重新走向导）。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// 配置键值表（BTreeMap：文件与内存镜像同构，序列化按键排序稳定）
pub type ConfigMap = BTreeMap<String, String>;

/// 配置文件名（锚点目录下）
pub const CONFIG_FILE: &str = "corpmail.toml";

/// 配置文件承载键（与 DB settings 同名，读写路由见 AppState::get/set_batch）
pub const CONFIG_KEYS: &[&str] = &[
    crate::state::keys::DATA_DIR,
    crate::state::keys::ACCOUNT_PREFIX,
    crate::state::keys::ACCOUNT_DOMAIN,
    crate::state::keys::DISPLAY_NAME,
    crate::state::keys::DISPLAY_NAME_AUTO,
    crate::state::keys::SMTP_HOST,
    crate::state::keys::SMTP_PORT,
    crate::state::keys::SMTP_SECURITY,
    crate::state::keys::RECV_HOST,
    crate::state::keys::RECV_PORT,
    crate::state::keys::RECV_SECURITY,
];

/// 键是否由配置文件承载
pub fn is_config_key(key: &str) -> bool {
    CONFIG_KEYS.contains(&key)
}

/// 读取全部配置（未知键过滤——配置文件内仅承载 CONFIG_KEYS）。
///
/// 三种情况分开处理：
/// - **文件不存在** → 空 map（未配置状态，正常首启）。
/// - **IO 失败**（权限、盘掉线、路径是目录）→ `Err`：这不是"没配置"，静默按未配置
///   启动会让用户面对空向导、甚至把默认值写回覆盖掉唯一可恢复的原文。调用方
///   （`AppState::open`）走启动失败提示。
/// - **解析失败**（手改坏、编码损坏、写一半）→ 原文改名备份后按未配置继续：
///   文件已留证可人工恢复，没必要因手误挡住整个应用。
/// - **解析失败且原文备份失败**（C16）→ `Err`：此时原文还留在原位且可能含
///   `data_dir_override`，若按未配置启动，向导保存会直接覆盖它，自定义数据
///   目录指针将永久丢失。这种情况下「静默自愈」比启动失败更危险。
///
/// `broken-*` 备份只保留最近 3 份，避免反复启动把锚点目录塞满。
pub fn load(anchor: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = anchor.join(CONFIG_FILE);
    // A8：按字节读 + 显式 UTF-8 校验。`read_to_string` 遇非法 UTF-8 返回
    // InvalidData，落进下方「IO 失败」的致命 Err——同一文件若是合法 UTF-8
    // 但 TOML 语法错反而能备份自愈，口径不一致。编码损坏与语法错同走
    // `broken_fallback`（备份 → 按未配置继续；备份失败 → Err，C16 不变）
    let text = match std::fs::read(&path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => return broken_fallback(anchor, &path, "文件编码异常（非 UTF-8）"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => {
            return Err(format!(
                "配置文件不可读（{}）：{e}。请检查文件权限或所在磁盘后重启",
                path.display()
            ));
        }
    };
    match toml::from_str::<BTreeMap<String, String>>(&text) {
        Ok(map) => Ok(map.into_iter().filter(|(k, _)| is_config_key(k)).collect()),
        Err(err) => broken_fallback(anchor, &path, &err.to_string()),
    }
}

/// 解析失败（TOML 语法错 / 编码损坏）的统一回退：原文改名备份后按未配置继续。
/// C16：备份失败时绝不能返回空 map。原文仍含可能有效的 data_dir 指针，
/// 按「未配置」走完向导后 save() 会原子覆盖它，用户唯一的目录指针就没了。
/// 与「IO 失败」同口径返回 Err，由启动链给出明确提示，交用户处理权限
fn broken_fallback(
    anchor: &Path,
    path: &Path,
    reason: &str,
) -> Result<BTreeMap<String, String>, String> {
    // R17 P2：秒级 stamp 在同一秒内二次损坏时会覆盖上一份备份（Unix rename /
    // Windows MoveFileExW 均静默 REPLACE_EXISTING），可能抹掉唯一含有效
    // data_dir 指针的副本。加纳秒 + pid，且目标已存在时递增序号。
    // 纳秒补零到 9 位：`prune_broken_backups` 按文件名字典序保留最近 3 份，
    // 变长纳秒会把 ...-999-... 错排到 ...-1000-... 之后。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0));
    broken_fallback_at(anchor, path, reason, stamp, 1001)
}

/// 与 `broken_fallback` 同逻辑，但时间戳与候选数由调用方给定。
/// 测试用固定 stamp + `max_candidates=1` 预占唯一候选名制造 rename 失败（C16）；
/// 生产始终走当前时间与 base + 1000 个后缀。
fn broken_fallback_at(
    anchor: &Path,
    path: &Path,
    reason: &str,
    stamp: (u64, u32),
    max_candidates: u32,
) -> Result<BTreeMap<String, String>, String> {
    let base = format!(
        "{CONFIG_FILE}.broken-{}-{:09}-{}",
        stamp.0,
        stamp.1,
        std::process::id()
    );
    let mut backup = anchor.join(&base);
    let mut free = !backup.exists();
    for seq in 1..max_candidates.max(1) {
        if free {
            break;
        }
        backup = anchor.join(format!("{base}-{seq}"));
        free = !backup.exists();
    }
    // R19：循环只检查了 base..base-(n-2)，末位候选未经 exists() 就 rename，
    // 极端碰撞下会静默 REPLACE 已有备份（可能含唯一 data_dir 指针）。
    if !free {
        return Err(format!(
            "配置文件格式异常且原文备份失败（候选 {base} 及 {max_candidates} 个后缀均被占用）：为避免覆盖已有备份，已停止启动。请清理锚点目录后重试"
        ));
    }
    let kept = std::fs::rename(path, &backup).is_ok();
    eprintln!(
        "[conf] 配置文件解析失败（{reason}）；{}",
        if kept {
            format!("原文已备份为 {}，本次按未配置启动", backup.display())
        } else {
            "原文备份失败（保留原文件），已拒绝继续启动".to_string()
        }
    );
    if !kept {
        return Err(format!(
            "配置文件格式异常且原文备份失败（{}）：为避免向导覆盖仍含数据目录指针的原文，已停止启动。请检查文件权限或磁盘状态后重试",
            path.display()
        ));
    }
    prune_broken_backups(anchor, 3);
    Ok(BTreeMap::new())
}

/// 只保留最近 `keep` 份 `corpmail.toml.broken-<ts>` 备份（文件名里的时间戳即排序键）。
fn prune_broken_backups(anchor: &Path, keep: usize) {
    let prefix = format!("{CONFIG_FILE}.broken-");
    let Ok(entries) = std::fs::read_dir(anchor) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix))
        // 目录同名时只是 rename 失败留下的占位（测试预占 / 手工创建），
        // remove_file 对目录必失败并刷错误日志；清理阶段只认普通文件。
        .filter(|n| {
            std::fs::symlink_metadata(anchor.join(n))
                .map(|m| m.is_file())
                .unwrap_or(false)
        })
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort(); // 时间戳等宽数字串：字典序即时间序
    for name in &names[..names.len() - keep] {
        if let Err(e) = std::fs::remove_file(anchor.join(name)) {
            eprintln!("[conf] 旧备份清理失败（{name}）：{e}");
        }
    }
}

/// 原子保存（临时文件 + rename：防写一半崩溃留残片）。
/// BTreeMap 序列化为排序平铺键值；文件头附说明注释。
pub fn save(anchor: &Path, cfg: &BTreeMap<String, String>) -> Result<(), String> {
    let mut text = String::from(
        "# CorpMail 应用配置（独立于数据目录：迁移数据、删除旧库备份均不影响本文件）\n\
         # data_dir_override 为数据目录指针，留空表示使用默认目录；请勿删除本文件\n",
    );
    let body = toml::to_string(cfg).map_err(|e| format!("配置序列化失败：{e}"))?;
    text.push_str(&body);
    std::fs::create_dir_all(anchor).map_err(|e| format!("配置目录不可创建：{e}"))?;
    let path = anchor.join(CONFIG_FILE);
    // tmp 带 pid：并发写或多进程残留时不会互相覆盖；rename 前 sync_all 保证
    // 掉电时不留空文件（审计 P2-5 / P1-48）
    let tmp = anchor.join(format!("{CONFIG_FILE}.{}.tmp", std::process::id()));
    // R17 P2：旧实现只在 rename 失败时清理 tmp——write_all/sync_all 失败
    // （磁盘满/配额超）时残片留在锚点目录，PID 变化后无人回收、持续累积。
    // 统一走闭包：任何一步失败都清掉本次 tmp 再上抛。
    let outcome = (|| -> Result<(), String> {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("配置临时文件创建失败：{e}"))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("配置写入失败：{e}"))?;
        f.sync_all().map_err(|e| format!("配置刷盘失败：{e}"))?;
        drop(f);
        std::fs::rename(&tmp, &path).map_err(|e| format!("配置落盘失败：{e}"))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

#[cfg(test)]
#[path = "tests/conf.rs"]
mod tests;
