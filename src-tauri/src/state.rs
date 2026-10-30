//! 应用状态：SQLite 单连接（`Mutex` 内部可变性）+ 业务配置读写
//!
//! 设计 §2/§13-R6：全库走单一 `Mutex<Connection>`，WAL 模式读写不互阻。
//! 账号密码不落库（经 `secrets` 走 OS keychain，keyfile 回退，设计 §5.4）。
//!
//! 锁策略：全部 Mutex/RwLock 一律 `unwrap_or_else(|e| e.into_inner())` 恢复中毒锁。
//! 中毒只说明某次持锁写入被 panic 打断，锁内数据本身通常仍可用；沿用 `.expect`
//! 会让一次 panic 演变为全应用连锁 panic（审计 P2-11）。

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
    // 收件状态（M4，设计 §3 schema 注释"状态"节）
    pub const LAST_SYNC_AT: &str = "last_sync_at";
    pub const LAST_UIDVALIDITY: &str = "last_uidvalidity";
    pub const LAST_UID: &str = "last_uid";

    /// 自定义数据目录指针（存独立配置文件 corpmail.toml，随锚点目录常驻）
    pub const DATA_DIR: &str = "data_dir_override";
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
    /// 锚点目录（系统默认应用数据目录）：独立配置文件 corpmail.toml 在此，
    /// 不随数据目录迁移、不被备份删除影响（resolve_start_dir 亦从此定位）
    anchor_dir: PathBuf,
    /// 应用配置内存镜像（corpmail.toml；Arc 供发送 worker 每封取快照——
    /// 配置不落库后 dispatch 不能再从 conn 读 SMTP 参数）
    config: std::sync::Arc<Mutex<crate::conf::ConfigMap>>,
    /// 发送队列入口（M3：lib.rs 装配 start_send_worker 后有值；测试/无 worker 时为 None，仅状态机流转）
    send_tx: Mutex<Option<std::sync::mpsc::Sender<i64>>>,
    /// 收件状态（M4 §6.2：手动触发防抖/进行中态展示；Arc 供收件线程更新）。
    /// pub(crate)：路由层回归测试需注入运行中态（C5 拒绝重拉）
    pub(crate) fetch_state: std::sync::Arc<Mutex<FetchState>>,
    /// 活跃收件线程数（含看门狗复位后仍在挂起的旧轮）。看门狗只复位 UI 状态，
    /// 线程本身因 imap 无网络超时仍可能卡死——达到上限即拒绝新轮，
    /// 防止挂起线程 + TcpStream + SQLite 连接无限累积（审计 P1-5）
    pub(crate) active_rounds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// 收件状态快照（收件按钮 spinner/禁用与状态片段共用）
#[derive(Clone, Debug)]
pub struct FetchState {
    pub running: bool,
    /// 最近一轮新增封数
    pub new_count: i64,
    pub last_error: String,
    /// 刚完成（状态片段读到即清，路由层据此发 HX-Refresh 刷新列表）
    pub just_finished: bool,
    /// 轮次代际：每次开始新轮 +1；挂起轮的迟到 finish（epoch 不匹配）不得写状态
    pub epoch: u64,
    /// 当前轮开始时间（epoch 秒）：看门狗判定挂起依据
    pub started_at: i64,
    /// 本轮拉取进度（M6 分批入库）：已入库 / 应拉总数；正在首拉前为 0/0
    pub fetch_done: usize,
    pub fetch_total: usize,
}

/// 收件轮看门狗上限（秒）：imap 无网络超时，挂起轮超过此时长由轮询链复位，
/// 避免收件按钮永久卡死（挂起线程自身因 epoch 失配无法再污染新一轮状态）
const FETCH_WATCHDOG_SECS: i64 = 300;

/// 同时存在的收件线程上限（含看门狗已复位但线程仍挂起的旧轮）。
/// 取 2 = 允许一个挂起轮等网络自愈，再多即拒绝新轮——每轮占 1 个 TcpStream
/// 与 1 个 SQLite 连接，无限重试会在网络黑洞下持续泄漏资源
const MAX_CONCURRENT_FETCH_ROUNDS: usize = 2;

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
    /// 打开/初始化数据库（schema 幂等迁移在 db::open 内完成；锚点=数据目录本身，
    /// 适用于测试与未启用自定义目录的场景）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(data_dir: &Path) -> Self {
        Self::open_with_anchor(data_dir, data_dir)
    }

    /// 打开/初始化数据库（lib.rs 专用：锚点目录存独立配置文件 corpmail.toml）。
    /// 失败返回原因，由调用方决定降级或报错——生产路径不得 panic：
    /// 数据目录失效（外置盘拔出/权限变更）时旧实现的 `.expect` 会让进程直接退出，
    /// Windows GUI 子系统无控制台，用户看到的现象就是「双击没反应」。
    pub fn try_open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("数据目录无法创建（{}）：{e}", data_dir.display()))?;
        let conn = crate::db::open(&data_dir.join("corpmail.db"))
            .map_err(|e| format!("数据库无法打开（{}）：{e}", data_dir.display()))?;
        Ok(Self {
            db: Mutex::new(conn),
            data_dir: data_dir.to_path_buf(),
            anchor_dir: anchor_dir.to_path_buf(),
            config: std::sync::Arc::new(Mutex::new(crate::conf::load(anchor_dir))),
            send_tx: Mutex::new(None),
            fetch_state: std::sync::Arc::new(Mutex::new(FetchState {
                running: false,
                new_count: 0,
                last_error: String::new(),
                just_finished: false,
                epoch: 0,
                started_at: 0,
                fetch_done: 0,
                fetch_total: 0,
            })),
            active_rounds: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// 打开/初始化数据库（锚点=数据目录本身，测试便捷入口）。
    /// 生产入口为 `try_open_with_anchor`（返回 Result，不 panic）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Self {
        Self::try_open_with_anchor(data_dir, anchor_dir)
            .unwrap_or_else(|e| panic!("应用数据目录不可用：{e}"))
    }

    /// 启动目录解析（lib.rs setup 调用）：独立配置文件 corpmail.toml 的 data_dir_override
    /// 指针定位自定义数据目录；文件缺失/指针失效（目录被删、外置盘拔出）回退锚点目录。
    ///
    /// 有效性 = 目录存在且含 corpmail.db（迁移快照必有；空壳/失效目录不采纳，
    /// 安全回退锚点目录——迁移前的原数据就留在那里）
    pub fn resolve_start_dir(anchor: &Path) -> PathBuf {
        let usable = |p: &str| Path::new(p).join("corpmail.db").is_file();
        if let Some(p) = crate::conf::load(anchor)
            .get(keys::DATA_DIR)
            .cloned()
            .filter(|p| usable(p))
        {
            return PathBuf::from(p);
        }
        anchor.to_path_buf()
    }

    /// 迁移数据目录到用户选定位置（M5：在线快照 + 重启生效，避免热切换连接的并发风险）。
    /// 指针写入独立配置文件 corpmail.toml（随锚点目录常驻，不随数据迁移、
    /// 不被备份删除影响）；原目录保留迁移前的库/附件备份，可由用户在
    /// 存储页手动删除（delete_old_backup）。
    /// 迁移期间收件线程可能写入新附件导致极小竞态窗口（附件文件缺失但元数据在），
    /// 重启后孤儿回收不会破坏元数据，重收件可补——概率极低，取舍见设计 §5.2 注记
    pub fn move_data_dir(&self, dest: &str) -> Result<String, String> {
        // 去尾部斜杠保留根目录："/" 与 Windows "C:\\" 不得被清成非绝对路径
        let raw = dest.trim();
        let trimmed = raw.trim_end_matches(['/', '\\']);
        let dest_str = if trimmed.is_empty() {
            raw.to_string()
        } else if trimmed.len() == 2 && trimmed.ends_with(':') {
            format!("{trimmed}\\")
        } else {
            trimmed.to_string()
        };
        let dest_path = PathBuf::from(dest_str);
        if !dest_path.is_absolute() {
            return Err("请选择绝对路径目录".into());
        }
        let cur = &self.data_dir;
        if dest_path == *cur {
            return Err("目标目录与当前数据目录相同".into());
        }
        if cur.starts_with(&dest_path) || dest_path.starts_with(cur) {
            return Err("目标目录不能位于当前数据目录内（或反向包含）".into());
        }
        if dest_path.join("corpmail.db").exists() {
            return Err("目标目录已存在 corpmail.db，请选择空目录".into());
        }
        // 目标已有 attachments/ 时拒绝：复制是合并语义，一旦后续步骤失败，
        // 回滚无法区分"本次写入"与"用户原有文件"，只能整目录删除 → 误删用户数据
        if dest_path.join("attachments").exists() {
            return Err("目标目录已存在 attachments 目录，为免与已有文件混同请选择空目录".into());
        }
        std::fs::create_dir_all(&dest_path).map_err(|e| format!("目标目录无法创建：{e}"))?;
        // 写入探针：目标可写
        let probe = dest_path.join(".corpmail-write-probe");
        std::fs::write(&probe, b"ok").map_err(|e| format!("目标目录不可写：{e}"))?;
        let _ = std::fs::remove_file(&probe);

        // 1) 数据库在线快照（VACUUM INTO：WAL 安全的一致性副本，业务不中断）
        let new_db = dest_path.join("corpmail.db");
        self.with_conn(|conn| {
            conn.execute("VACUUM INTO ?1", [new_db.to_string_lossy().as_ref()])
                .map_err(|e| format!("数据库快照失败：{e}"))
        })?;

        // 2) 附件复制 + 3) 指针写入：任一步失败都回滚已产出物（快照库/附件副本），
        //    并恢复内存镜像中的指针——保证调用方（向导/存储页）原地重试不被
        //    "目标已存在 corpmail.db"拦截（C6 半保存态修复）
        let src_att = cur.join("attachments");
        // 本次实际写入的文件清单：回滚只删这些，不碰目标目录里任何既有内容
        let mut written: Vec<PathBuf> = Vec::new();
        let migrated: Result<(), String> = (|| {
            if src_att.is_dir() {
                copy_dir_recursive(&src_att, &dest_path.join("attachments"), &mut written)
                    .map_err(|e| format!("附件目录复制失败：{e}"))?;
            }
            let (prev, save_res) = {
                let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                let prev = cfg.get(keys::DATA_DIR).cloned();
                cfg.insert(
                    keys::DATA_DIR.to_string(),
                    dest_path.to_string_lossy().into_owned(),
                );
                let snapshot = cfg.clone();
                (prev, crate::conf::save(&self.anchor_dir, &snapshot))
            };
            save_res.map_err(|e| {
                // 配置写失败：镜像恢复指针（镜像必须反映文件内容）
                let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                match prev {
                    Some(v) => {
                        cfg.insert(keys::DATA_DIR.to_string(), v);
                    }
                    None => {
                        cfg.remove(keys::DATA_DIR);
                    }
                }
                format!("配置文件写入失败：{e}")
            })
        })();
        if let Err(e) = migrated {
            let _ = std::fs::remove_file(&new_db);
            // 精确回滚：只删本次复制出的文件，再回收本次创建的空目录。
            // 旧实现对 dest/attachments 直接 remove_dir_all——目标目录里用户
            // 原有的附件会被连带删除（指针写入失败即触发，破坏不可逆）
            for f in &written {
                let _ = std::fs::remove_file(f);
            }
            crate::mail::prune_empty_dirs(&dest_path.join("attachments"));
            return Err(e);
        }

        Ok(format!(
            "数据已迁移到新目录，重启应用后生效。旧目录中的数据库备份可在 设置 → 存储管理 中手动删除。新目录：{}",
            dest_path.display()
        ))
    }

    /// 迁移备份统计：锚点（默认安装目录）中的 corpmail.db[-wal/-shm] + attachments/。
    /// 仅当已迁移（data_dir != anchor）且锚点存在备份文件时为 Some
    pub fn old_backup_stats(&self) -> Option<(u64, u64)> {
        if self.data_dir == self.anchor_dir {
            return None;
        }
        let mut bytes = 0u64;
        let mut files = 0u64;
        for f in ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"] {
            if let Ok(md) = std::fs::metadata(self.anchor_dir.join(f))
                && md.is_file()
            {
                bytes += md.len();
                files += 1;
            }
        }
        let (b, n) = dir_stats(&self.anchor_dir.join("attachments"));
        bytes += b;
        files += n;
        (files > 0).then_some((bytes, files))
    }

    /// 删除迁移备份（存储页按钮，验收功能）：清锚点目录中的 corpmail.db[-wal/-shm]
    /// 与 attachments/；配置文件 corpmail.toml 与当前数据不受影响。
    /// 返回 (释放字节, 删除文件数)
    pub fn delete_old_backup(&self) -> Result<(u64, u64), String> {
        // 迁移尚未重启生效：本进程仍直连旧目录库，删掉它会让后续收件/删信写进
        // 已 unlink 的 inode——重启后新目录只剩迁移时的快照，期间变更全部丢失。
        // 必须先重启（指针生效、data_dir 变为新目录）才允许清理。
        // 放在"默认目录"判断之前：此时 data_dir 仍等于锚点，否则会误报"无迁移备份"
        if self.pending_data_dir().is_some() {
            return Err("数据目录迁移尚未重启生效，请先重启应用再删除旧目录备份".into());
        }
        if self.data_dir == self.anchor_dir {
            return Err("当前使用默认数据目录，无迁移备份".into());
        }
        if !self.data_dir.join("corpmail.db").is_file() {
            return Err("当前数据目录缺少数据库文件，拒绝删除备份".into());
        }
        if self.old_backup_stats().is_none() {
            return Err("未发现可删除的迁移备份".into());
        }
        let mut freed = 0u64;
        let mut files = 0u64;
        for f in ["corpmail.db", "corpmail.db-wal", "corpmail.db-shm"] {
            let p = self.anchor_dir.join(f);
            if p.is_file() {
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(&p).map_err(|e| format!("删除 {f} 失败：{e}"))?;
                freed += sz;
                files += 1;
            }
        }
        let att = self.anchor_dir.join("attachments");
        if att.is_dir() {
            let (b, n) = dir_stats(&att);
            std::fs::remove_dir_all(&att).map_err(|e| format!("删除备份附件目录失败：{e}"))?;
            freed += b;
            files += n;
        }
        Ok((freed, files))
    }

    /// 收件状态快照（克隆；just_finished 读取后由 clear_fetch_finish 显式清除）
    pub fn fetch_snapshot(&self) -> FetchState {
        self.fetch_guard_hung();
        self.fetch_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 看门狗：挂起轮（超出 FETCH_WATCHDOG_SECS 无进展）强制复位。
    /// 由 fetch_snapshot（状态轮询链，进行中 2s 一次）驱动。复位即 epoch+1，
    /// 挂起线程即便之后网络恢复，其 finish 也会因 epoch 失配被丢弃。
    fn fetch_guard_hung(&self) {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running && crate::mail::now_epoch() - s.started_at > FETCH_WATCHDOG_SECS {
            eprintln!("[fetch] 看门狗：收件轮超 {FETCH_WATCHDOG_SECS}s 无进展，强制复位");
            s.running = false;
            s.epoch += 1;
            s.last_error = format!(
                "收件超时（{} 分钟无进展，可能网络中断），已复位，可重试",
                FETCH_WATCHDOG_SECS / 60
            );
            s.just_finished = true;
        }
    }

    /// 清除"刚完成"标记（状态片段消费后调用，避免重复触发列表刷新）
    pub fn clear_fetch_finish(&self) {
        self.fetch_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .just_finished = false;
    }

    /// 启动一轮收件（§6.2：手动/启动/定时共用；进行中重复触发直接忽略——防抖）。
    ///
    /// 参数快照与状态 CAS 在调用线程完成后，网络拉取与入库交由独立线程执行
    /// （独立连接，同发送 worker 模式；完成后更新游标/last_sync_at 并置 just_finished）。
    pub fn start_fetch(&self) {
        // 挂起轮上限（审计 P1-5）：看门狗只复位 UI 状态，挂起线程仍在跑。
        // 达到上限即拒绝新轮并给出明确原因，而不是继续叠加线程
        if self.active_rounds.load(std::sync::atomic::Ordering::SeqCst)
            >= MAX_CONCURRENT_FETCH_ROUNDS
        {
            self.set_fetch_error(format!(
                "已有 {MAX_CONCURRENT_FETCH_ROUNDS} 轮收件在后台无响应（网络可能中断），\
                 请检查网络后重启应用"
            ));
            return;
        }
        let Some(epoch) = self.fetch_cas_begin() else {
            return; // 已在收件中（防抖 §6.2）
        };
        let d = defaults();
        let g = |k: &str| self.get_or(k, &d).unwrap_or_default();
        let host = g(keys::RECV_HOST);
        let port: u16 = g(keys::RECV_PORT).parse().unwrap_or(993);
        let account = full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN));
        let password = match self.retrieve_password(&account) {
            crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
            crate::secrets::GetResult::Missing => {
                self.fetch_end(epoch, 0, "凭据缺失，请先在设置中保存密码");
                return;
            }
        };
        let first_days = first_fetch_days_from_raw(&g(keys::FIRST_FETCH_DAYS));
        let last = crate::mail::LastCursor {
            uidvalidity: g(keys::LAST_UIDVALIDITY).parse().unwrap_or(0),
            uid: g(keys::LAST_UID).parse().unwrap_or(0),
        };
        let fetch_state = std::sync::Arc::clone(&self.fetch_state);
        let active_rounds = std::sync::Arc::clone(&self.active_rounds);
        let db_path = self.data_dir.join("corpmail.db");
        let data_dir = self.data_dir.clone();
        active_rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::spawn(move || {
            // RAII：任何退出路径（含 panic 越过 fetch_round 内部 catch_unwind）都归还名额
            let _guard = RoundGuard(active_rounds);
            fetch_round(FetchJob {
                fetch_state,
                db_path,
                data_dir,
                host,
                port,
                account,
                password,
                first_days,
                last,
                epoch,
            });
        });
    }

    /// 写入一条收件错误（不改变 running：正在收件时由收件线程负责收尾）
    fn set_fetch_error(&self, msg: String) {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return;
        }
        s.last_error = msg;
        s.just_finished = true;
    }

    /// 收件状态 CAS：空闲→进行中（成功 true；已在收件返回 false）
    /// 开始新轮（CAS）：成功返回该轮 epoch（finish 写状态的凭证）
    fn fetch_cas_begin(&self) -> Option<u64> {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return None;
        }
        s.running = true;
        s.just_finished = false;
        s.epoch += 1;
        s.started_at = crate::mail::now_epoch();
        s.last_error.clear();
        s.fetch_done = 0;
        s.fetch_total = 0;
        Some(s.epoch)
    }

    /// 收件结束（凭据缺失等同步失败路径；线程路径在闭包内直接更新 Arc）
    /// 同步失败收尾（start_fetch 线程前路径）。带 epoch 凭证校验
    fn fetch_end(&self, epoch: u64, new_count: i64, error: &str) {
        let mut s = self.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch != epoch {
            return; // 看门狗已复位或新一轮已开始：迟到结果丢弃
        }
        s.running = false;
        s.new_count = new_count;
        s.last_error = error.into();
        s.just_finished = true;
    }

    /// 启动发送 worker（M3，设计 §2 单飞线程；生产装配于 lib.rs，测试不启动）。
    ///
    /// 独立连接 + MPSC 队列逐封派发（企业 SMTP 串行更稳）；
    /// 启动时扫描残留 queued 一并补发（崩溃恢复）。
    /// 发送队列装配：启动残留 queued 补发 + 后续入队即时派发。
    /// worker 持独立连接与配置镜像 Arc（配置源 corpmail.toml，不占库锁）
    pub fn start_send_worker(&self) {
        let mut guard = self.send_tx.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        let Ok(conn) = crate::db::open(&self.data_dir.join("corpmail.db")) else {
            return;
        };
        let config_arc = std::sync::Arc::clone(&self.config);
        // 附件根目录：worker 线程独立取路径（不持 AppState；附件随信发送时读盘解压）
        let attach_root = self.data_dir.join("attachments");
        let (tx, rx) = std::sync::mpsc::channel::<i64>();
        // 崩溃恢复 1/2：`sending` 是仅存在于进程内的中间态，跨进程无合法含义。
        // 进程在发送中被杀（关机/崩溃/强退）后该邮件会永久停在 sending：
        // 启动补发只查 queued、reopen_failed 只收 failed、详情页 2s 轮询永不停止，
        // 用户既不能重发也不能删除重来。这里复位为 failed 并写明原因，把决策权交回用户
        let recovered = crate::mail::recover_interrupted_sends(&conn);
        if recovered > 0 {
            eprintln!("[send] 启动自愈：{recovered} 封中断的 sending 邮件已复位为 failed");
        }
        // 崩溃恢复 2/2：残留 queued 补发（按 id 升序）
        let stale: Vec<i64> = conn
            .prepare("SELECT id FROM emails WHERE status = 'queued' ORDER BY id")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, i64>(0))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        std::thread::spawn(move || {
            for id in rx {
                // 发送参数快照：每封派发前重读配置（corpmail.toml；改动即时生效，
                // 不落库——验收拍板简化）
                let smtp = {
                    let cfg = config_arc.lock().unwrap_or_else(|e| e.into_inner());
                    crate::mail::SmtpConfig::from_config(&cfg)
                };
                // 派发内部所有终态（含各类失败）均已落库；此处 Err 仅剩
                // "抢占失败/读库异常"两条管理性路径，记日志供排查即可。
                // catch_unwind：lettre/MIME 编码等任何 panic 不得打死 worker 线程
                // ——worker 一死 channel 关闭，此后所有 enqueue_send 静默无效，
                // 邮件停在 queued 直到重启（审计 P2-16）
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::mail::dispatch_one(
                        &conn,
                        &crate::mail::RealSender,
                        id,
                        &smtp,
                        &attach_root,
                    )
                }));
                match outcome {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => eprintln!("[send] 邮件 {id} 派发未执行：{err}"),
                    Err(_) => {
                        eprintln!("[send] 邮件 {id} 派发 panic（已捕获，标记失败后继续）");
                        // 已抢占到 sending 的邮件必须落定，否则只能等下次启动自愈
                        let _ = conn.execute(
                            "UPDATE emails SET status = 'failed',
                                    error = '发送过程异常中断，请重发'
                             WHERE id = ?1 AND status = 'sending'",
                            [id],
                        );
                    }
                }
            }
        });
        for id in stale {
            let _ = tx.send(id);
        }
        *guard = Some(tx);
    }

    /// 入队发送（无 worker 时为空操作：邮件停留 queued 态，下次启动补发）
    pub fn enqueue_send(&self, id: i64) {
        if let Some(tx) = self
            .send_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(id);
        }
    }

    /// 是否已完成首启配置（向导判定依据：账号前缀已写入）
    pub fn configured(&self) -> bool {
        self.get(keys::ACCOUNT_PREFIX)
            .map(|v| v.filter(|s| !s.is_empty()).is_some())
            .unwrap_or(false)
    }

    /// 借出连接执行闭包（邮件存储层 mail 模块使用；锁随闭包返回释放）
    pub fn with_conn<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }

    /// 配置镜像快照（外部线程/测试读 corpmail.toml 内存镜像用；worker 线程直接持 Arc）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn config_snapshot(&self) -> crate::conf::ConfigMap {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 读键。配置键（conf::CONFIG_KEYS：数据目录/服务器/账号）仅从 corpmail.toml
    /// 内存镜像读（文件为唯一持久源，不落库）；其余读库
    pub fn get(&self, key: &str) -> Option<Option<String>> {
        if crate::conf::is_config_key(key) {
            let cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            return Some(cfg.get(key).cloned());
        }
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
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

    /// 批量写。路由拆分（验收拍板简化）：配置键仅写独立配置文件 corpmail.toml
    /// （先写文件，成功后才处理库键）；非配置键走库单事务（全库单一写入口不变）
    pub fn set_batch(&self, pairs: &[(&str, &str)]) -> Result<(), String> {
        let cfg_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .copied()
            .filter(|(k, _)| crate::conf::is_config_key(k))
            .collect();
        let db_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .copied()
            .filter(|(k, _)| !crate::conf::is_config_key(k))
            .collect();
        if !cfg_pairs.is_empty() {
            // 写前快照旧值：文件写失败时恢复原镜像（覆盖写失败不得丢掉旧值）
            let prev: Vec<(String, Option<String>)> = {
                let cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                cfg_pairs
                    .iter()
                    .map(|(k, _)| ((*k).to_string(), cfg.get(*k).cloned()))
                    .collect()
            };
            {
                let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                for (k, v) in &cfg_pairs {
                    cfg.insert((*k).to_string(), (*v).to_string());
                }
            }
            let save_res = {
                let cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                crate::conf::save(&self.anchor_dir, &cfg)
            };
            if let Err(e) = save_res {
                let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
                for (k, old) in prev {
                    match old {
                        Some(v) => {
                            cfg.insert(k, v);
                        }
                        None => {
                            cfg.remove(&k);
                        }
                    }
                }
                return Err(format!("配置文件写入失败：{e}"));
            }
        }
        if !db_pairs.is_empty() {
            let mut conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            for (k, v) in &db_pairs {
                tx.execute(
                    "INSERT INTO settings(k, v) VALUES (?1, ?2)
                     ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                    [k, v],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 数据库文件大小（字节；设置·存储页"邮箱数据库大小"；含 WAL 与索引开销）
    pub fn db_size_bytes(&self) -> u64 {
        ["corpmail.db", "corpmail.db-wal"]
            .iter()
            .filter_map(|f| std::fs::metadata(self.data_dir.join(f)).ok())
            .map(|m| m.len())
            .sum()
    }

    /// SQLite 版本串（关于页展示）
    pub fn sqlite_version(&self) -> String {
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))
            .unwrap_or_default()
    }

    /// 侧栏文件夹计数（列表屏冗余展示）：单条聚合 SQL 一次扫表（5 个计数）
    pub fn folder_counts(&self) -> HashMap<&'static str, i64> {
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn.query_row(
            "SELECT COALESCE(SUM(folder = 'inbox'), 0),
                    COALESCE(SUM(folder = 'inbox' AND unread = 1), 0),
                    count(*),
                    COALESCE(SUM(folder = 'sent'), 0),
                    COALESCE(SUM(folder = 'draft'), 0)
             FROM emails",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        );
        let (inbox, inbox_unread, all, sent, draft) = row.unwrap_or((0, 0, 0, 0, 0));
        HashMap::from([
            ("inbox", inbox),
            ("inbox_unread", inbox_unread),
            ("all", all),
            ("sent", sent),
            ("draft", draft),
        ])
    }

    /// 存密码（secrets 封装；返回是否发生 keyfile 降级）
    pub fn store_password(&self, account: &str, password: &str) -> Result<bool, String> {
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        crate::secrets::store(&conn, account, password)
    }

    /// 读密码（keychain 优先，keyfile 回退）。
    /// 命中进程内缓存时不取 DB 锁：keychain 授权窗口可能长等，不能连带阻塞 UI
    pub fn retrieve_password(&self, account: &str) -> crate::secrets::GetResult {
        if let Some(hit) = crate::secrets::cached(account) {
            return hit;
        }
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        crate::secrets::retrieve(&conn, account)
    }

    /// 删除密码（账号变更迁移时用；两条路径都清，幂等）
    pub fn clear_password(&self, account: &str) {
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        crate::secrets::clear(&conn, account);
    }

    /// keychain 是否处于 keyfile 降级态（设置·账号提示用）
    pub fn secrets_degraded(&self, account: &str) -> bool {
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        crate::secrets::degraded(&conn, account)
    }

    /// 待生效的数据目录（M5 迁移）：读 corpmail.toml 指针；
    /// 重启后 resolve_start_dir 应用指针、其值 == 当前目录，提示自然消失
    pub fn pending_data_dir(&self) -> Option<String> {
        let over = match self.get(keys::DATA_DIR) {
            Some(Some(v)) if !v.trim().is_empty() => v,
            _ => return None,
        };
        let pending = PathBuf::from(&over);
        (pending != self.data_dir && pending.join("corpmail.db").is_file()).then_some(over)
    }

    /// 数据目录下的独立附件占用（目录不存在 → 0）：(字节, 文件数)
    pub fn attachments_stats(&self) -> (u64, u64) {
        dir_stats(&self.data_dir.join("attachments"))
    }
}

/// 目录递归统计：(字节, 文件数)；目录不存在 → (0, 0)（附件占用与迁移备份共用）
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

/// 递归复制目录（M5 迁移用：附件目录两级结构 {YYYY-MM}/{email_id}/ 文件）；
/// 目标已存在的同名文件覆盖，缺失目录逐级创建。写入成功的目标路径追加进
/// `written`，供失败回滚精确删除本次产出物（不误伤目标目录既有文件）
fn copy_dir_recursive(src: &Path, dest: &Path, written: &mut Vec<PathBuf>) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("创建目录失败：{e}"))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("读取目录失败：{e}"))? {
        let entry = entry.map_err(|e| format!("遍历失败：{e}"))?;
        let ty = entry
            .file_type()
            .map_err(|e| format!("读取类型失败：{e}"))?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to, written)?;
        } else {
            std::fs::copy(entry.path(), &to).map_err(|e| format!("复制文件失败：{e}"))?;
            written.push(to);
        }
    }
    Ok(())
}

/// 首拉天数解析：0 = 全部（UID SEARCH ALL，合法值，不得作非法过滤）；
/// >0 = SINCE 天数（7/30/90 等）；解析失败（未配置/非法）兜底默认 30（与 defaults 一致）。
///
/// 修复：旧实现 `.parse().ok().filter(|v| *v > 0).unwrap_or(7)` 把"全部"(=0)
/// 误判为非法并回退到 7 天，导致用户在设置里选"全部"实际只拉近 7 天邮件。
fn first_fetch_days_from_raw(raw: &str) -> i64 {
    raw.parse::<i64>().unwrap_or(30)
}

/// 收件线程名额归还（Drop 时递减：正常结束、提前 return、panic 展开均生效）
struct RoundGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for RoundGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 收件轮次任务参数（start_fetch 快照 → fetch_round 线程体）
struct FetchJob {
    fetch_state: std::sync::Arc<Mutex<FetchState>>,
    db_path: PathBuf,
    data_dir: PathBuf,
    host: String,
    port: u16,
    account: String,
    password: String,
    first_days: i64,
    last: crate::mail::LastCursor,
    /// 本轮代际（fetch_cas_begin 发放；finish 时的凭证）
    epoch: u64,
}

/// 收件轮次主体（start_fetch 线程执行体）：拉取 → 入库 → 游标推进 → 状态复位。
/// 独立连接访问数据库（与主连接、发送 worker 并行，WAL 读写不互阻）。
/// 收件游标持久化（需求2 断点续传；C4 并发加固）：单事务写 uidvalidity/uid
/// （可选 last_sync_at）。单调性以 DB 侧现值为准——内存快照看不见其它轮已写值，
/// 看门狗复位后的旧轮与本轮并发写时，若用内存快照判断会用旧游标改写新轮已推进的值。
/// uidvalidity 变化时 UID 空间重置，uid 无条件采纳（允许变小）。
/// /// 顺序约束：调用方必须先成功入库再调本函数——“游标只越过已入库邮件”
/// 是断点不漏信的根基。
fn persist_fetch_cursor(
    conn: &mut Connection,
    uidvalidity: u32,
    uid: u32,
    sync_at: bool,
) -> Result<(), String> {
    let tx = conn
        .transaction()
        .map_err(|e| format!("游标事务开启失败：{e}"))?;
    let cur_uv: u32 = crate::db::settings::get(&tx, keys::LAST_UIDVALIDITY)
        .map_err(|e| format!("uidvalidity 读取失败：{e}"))?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cur_uid: u32 = crate::db::settings::get(&tx, keys::LAST_UID)
        .map_err(|e| format!("uid 读取失败：{e}"))?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    crate::db::settings::set(&tx, keys::LAST_UIDVALIDITY, &uidvalidity.to_string())
        .map_err(|e| format!("uidvalidity 写入失败：{e}"))?;
    if uid > 0 && (uidvalidity != cur_uv || uid > cur_uid) {
        crate::db::settings::set(&tx, keys::LAST_UID, &uid.to_string())
            .map_err(|e| format!("uid 写入失败：{e}"))?;
    }
    if sync_at {
        crate::db::settings::set(
            &tx,
            keys::LAST_SYNC_AT,
            &crate::mail::now_epoch().to_string(),
        )
        .map_err(|e| format!("last_sync_at 写入失败：{e}"))?;
    }
    tx.commit().map_err(|e| format!("游标提交失败：{e}"))
}

fn fetch_round(job: FetchJob) {
    let FetchJob {
        fetch_state,
        db_path,
        data_dir,
        host,
        port,
        account,
        password,
        first_days,
        last,
        epoch,
    } = job;
    // finish 携带轮次凭证：epoch 失配（看门狗已复位/新一轮已开始）则仅记日志不写状态，
    // 防止挂起轮迟到完成污染当前轮的展示
    let finish = |new_count: i64, error: String| {
        let mut s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch != epoch {
            eprintln!("[fetch] 迟到结果（epoch {epoch} ≠ {}）丢弃", s.epoch);
            return;
        }
        s.running = false;
        s.new_count = new_count;
        s.last_error = error;
        s.just_finished = true;
    };
    let Ok(mut conn) = crate::db::open(&db_path) else {
        finish(0, "本地数据库打开失败".into());
        return;
    };
    // 网络探活（P1-5）：imap 无连接超时接口，先做一次有界 TCP 连接，
    // 把"根本不可达"（内网设备丢包/DNS 错误/端口不通）的轮次在 10s 内结束。
    // 探活成功不代表协议可用，后续握手仍由 imap 完成
    if let Err(e) = crate::net::probe_tcp(&host, port) {
        finish(0, format!("{e}；本轮已终止（服务器不可达）"));
        return;
    }
    // 收件主体 catch_unwind（M5 验收反馈）：imap 3.0.0-alpha 解析、入库等任何 panic
    // 不得中断线程——否则 running 永久卡 true，手动收取按钮 CAS 永远被拒，
    // 用户表现为"无法刷新"。捕获后状态复位并把错误透出给 UI。
    let mut added = 0usize;
    let outcome: Result<(u32, u32), String> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // 分批拉取（M6）：每批解析后立即入库并推进进度，边拉边显示；
            // 入库与网络拉取同属关键路径，任一 panic 都必须复位状态。
            let mut on_batch = |mails: &[crate::mail::IncomingMail],
                                done: usize,
                                total: usize,
                                uidvalidity: u32,
                                batch_max: u32|
             -> Result<(), String> {
                // C4：看门狗已复位/新一轮已开始 → 立即终止本轮（Err 会中断
                // fetch_batched 后续批次下载），防止旧轮维续写库与游标
                {
                    let s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
                    if s.epoch != epoch {
                        return Err("收件轮已中止".into());
                    }
                }
                added += crate::mail::store_incoming_conn(&mut conn, &data_dir, mails)?;
                // 需求2 断点续传：每批成功入库后立即持久化游标到本批最大 UID。
                // 崩溃/退出后重启，last.uid 已推进，下一轮增量续拉，不重不漏；
                // 本批入库失败会提前 ? 返回，游标不推进，靠去重兜底。
                // C4：单调性以 DB 侧现值为准，防旧轮与新轮并发回退。
                persist_fetch_cursor(&mut conn, uidvalidity, batch_max, false)?;
                let mut s = fetch_state.lock().unwrap_or_else(|e| e.into_inner());
                if s.epoch == epoch {
                    s.fetch_done = done;
                    s.fetch_total = total;
                }
                Ok(())
            };
            crate::mail::RealFetcher.fetch_batched(
                &host,
                port,
                &account,
                &password,
                first_days,
                last,
                &mut on_batch,
            )
        }))
        .unwrap_or_else(|_| {
            eprintln!("[fetch] 收件线程 panic（已捕获，状态复位）");
            Err("收件内部异常终止，请重试收取".into())
        });
    match outcome {
        Ok((uidvalidity, max_uid)) => {
            // 收尾游标（与每批持久化共用同一 DB 侧单调实现，C4）：
            // max_uid 覆盖解析失败/去重跳过的邮件；并写 last_sync_at。
            // 失败仅记日志——本轮邮件已入库，下轮重拉去重兜底。
            if let Err(err) = persist_fetch_cursor(&mut conn, uidvalidity, max_uid, true) {
                eprintln!("[fetch] 游标写入失败（本轮邮件已入库，下轮重拉去重兜底）：{err}");
            }
            eprintln!(
                "[fetch] 完成：命中游标 uid={}，入库 {added} 封（分批持久化）",
                max_uid
            );
            finish(added as i64, String::new());
        }
        Err(e) => {
            eprintln!("[fetch] 失败：{e}");
            finish(0, e);
        }
    }
}

#[cfg(test)]
#[path = "tests/state.rs"]
mod tests;
