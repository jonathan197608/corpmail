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
    // 收件状态（M4，设计 §3 schema 注释"状态"节）
    pub const LAST_SYNC_AT: &str = "last_sync_at";
    pub const LAST_UIDVALIDITY: &str = "last_uidvalidity";
    pub const LAST_UID: &str = "last_uid";

    /// 自定义数据目录指针（存独立配置文件 mailbox.toml，随锚点目录常驻）
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
    /// 锚点目录（系统默认应用数据目录）：独立配置文件 mailbox.toml 在此，
    /// 不随数据目录迁移、不被备份删除影响（resolve_start_dir 亦从此定位）
    anchor_dir: PathBuf,
    /// 应用配置内存镜像（mailbox.toml；Arc 供发送 worker 每封取快照——
    /// 配置不落库后 dispatch 不能再从 conn 读 SMTP 参数）
    config: std::sync::Arc<Mutex<crate::conf::ConfigMap>>,
    /// 发送队列入口（M3：lib.rs 装配 start_send_worker 后有值；测试/无 worker 时为 None，仅状态机流转）
    send_tx: Mutex<Option<std::sync::mpsc::Sender<i64>>>,
    /// 收件状态（M4 §6.2：手动触发防抖/进行中态展示；Arc 供收件线程更新）
    fetch_state: std::sync::Arc<Mutex<FetchState>>,
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
}

/// 收件轮看门狗上限（秒）：imap 无网络超时，挂起轮超过此时长由轮询链复位，
/// 避免收件按钮永久卡死（挂起线程自身因 epoch 失配无法再污染新一轮状态）
const FETCH_WATCHDOG_SECS: i64 = 300;

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
    /// 适用于测试与未启用自定义目录的场景。生产入口为 open_with_anchor）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(data_dir: &Path) -> Self {
        Self::open_with_anchor(data_dir, data_dir)
    }

    /// 打开/初始化数据库（lib.rs 专用：锚点目录存独立配置文件 mailbox.toml）。
    /// 配置直接从文件加载（唯一持久源，不做任何旧库回填）；文件为空即未配置
    pub fn open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Self {
        std::fs::create_dir_all(data_dir).expect("数据目录应可创建");
        let conn = crate::db::open(&data_dir.join("mailbox.db")).expect("数据库应可打开");
        Self {
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
            })),
        }
    }

    /// 启动目录解析（lib.rs setup 调用）：独立配置文件 mailbox.toml 的 data_dir_override
    /// 指针定位自定义数据目录；文件缺失/指针失效（目录被删、外置盘拔出）回退锚点目录。
    ///
    /// 有效性 = 目录存在且含 mailbox.db（迁移快照必有；空壳/失效目录不采纳，
    /// 安全回退锚点目录——迁移前的原数据就留在那里）
    pub fn resolve_start_dir(anchor: &Path) -> PathBuf {
        let usable = |p: &str| Path::new(p).join("mailbox.db").is_file();
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
    /// 指针写入独立配置文件 mailbox.toml（随锚点目录常驻，不随数据迁移、
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
        if dest_path.join("mailbox.db").exists() {
            return Err("目标目录已存在 mailbox.db，请选择空目录".into());
        }
        std::fs::create_dir_all(&dest_path).map_err(|e| format!("目标目录无法创建：{e}"))?;
        // 写入探针：目标可写
        let probe = dest_path.join(".mailbox-write-probe");
        std::fs::write(&probe, b"ok").map_err(|e| format!("目标目录不可写：{e}"))?;
        let _ = std::fs::remove_file(&probe);

        // 1) 数据库在线快照（VACUUM INTO：WAL 安全的一致性副本，业务不中断）
        let new_db = dest_path.join("mailbox.db");
        self.with_conn(|conn| {
            conn.execute("VACUUM INTO ?1", [new_db.to_string_lossy().as_ref()])
                .map_err(|e| format!("数据库快照失败：{e}"))
        })?;

        // 2) 附件目录复制（大附件独立文件；崩溃中断则目标残留，下次迁移前有 mailbox.db 检查拦住）
        let src_att = cur.join("attachments");
        if src_att.is_dir() {
            copy_dir_recursive(&src_att, &dest_path.join("attachments"))
                .map_err(|e| format!("附件目录复制失败：{e}"))?;
        }

        // 3) 指针写入独立配置文件（mailbox.toml，唯一持久源；不落库）。
        //    配置写失败 → 清理已产出的目标快照与附件目录（保证重试不被"已存在库"拦截）
        {
            let mut cfg = self.config.lock().expect("配置锁中毒");
            cfg.insert(
                keys::DATA_DIR.to_string(),
                dest_path.to_string_lossy().into_owned(),
            );
            let snapshot = cfg.clone();
            drop(cfg);
            if let Err(e) = crate::conf::save(&self.anchor_dir, &snapshot) {
                let _ = std::fs::remove_file(&new_db);
                if dest_path.join("attachments").is_dir() {
                    let _ = std::fs::remove_dir_all(dest_path.join("attachments"));
                }
                return Err(format!("配置文件写入失败：{e}"));
            }
        }

        Ok(format!(
            "数据已迁移到新目录，重启应用后生效。旧目录中的数据库备份可在 设置 → 存储管理 中手动删除。新目录：{}",
            dest_path.display()
        ))
    }

    /// 迁移备份统计：锚点（默认安装目录）中的 mailbox.db[-wal/-shm] + attachments/。
    /// 仅当已迁移（data_dir != anchor）且锚点存在备份文件时为 Some
    pub fn old_backup_stats(&self) -> Option<(u64, u64)> {
        if self.data_dir == self.anchor_dir {
            return None;
        }
        let mut bytes = 0u64;
        let mut files = 0u64;
        for f in ["mailbox.db", "mailbox.db-wal", "mailbox.db-shm"] {
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

    /// 删除迁移备份（存储页按钮，验收功能）：清锚点目录中的 mailbox.db[-wal/-shm]
    /// 与 attachments/；配置文件 mailbox.toml 与当前数据不受影响。
    /// 返回 (释放字节, 删除文件数)
    pub fn delete_old_backup(&self) -> Result<(u64, u64), String> {
        if self.data_dir == self.anchor_dir {
            return Err("当前使用默认数据目录，无迁移备份".into());
        }
        if !self.data_dir.join("mailbox.db").is_file() {
            return Err("当前数据目录缺少数据库文件，拒绝删除备份".into());
        }
        if self.old_backup_stats().is_none() {
            return Err("未发现可删除的迁移备份".into());
        }
        let mut freed = 0u64;
        let mut files = 0u64;
        for f in ["mailbox.db", "mailbox.db-wal", "mailbox.db-shm"] {
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
        self.fetch_state.lock().expect("收件状态锁中毒").clone()
    }

    /// 看门狗：挂起轮（超出 FETCH_WATCHDOG_SECS 无进展）强制复位。
    /// 由 fetch_snapshot（状态轮询链，进行中 2s 一次）驱动。复位即 epoch+1，
    /// 挂起线程即便之后网络恢复，其 finish 也会因 epoch 失配被丢弃。
    fn fetch_guard_hung(&self) {
        let mut s = self.fetch_state.lock().expect("收件状态锁中毒");
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
            .expect("收件状态锁中毒")
            .just_finished = false;
    }

    /// 启动一轮收件（§6.2：手动/启动/定时共用；进行中重复触发直接忽略——防抖）。
    ///
    /// 参数快照与状态 CAS 在调用线程完成后，网络拉取与入库交由独立线程执行
    /// （独立连接，同发送 worker 模式；完成后更新游标/last_sync_at 并置 just_finished）。
    pub fn start_fetch(&self) {
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
        let first_days = g(keys::FIRST_FETCH_DAYS)
            .parse::<i64>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or(7);
        let last = crate::mail::LastCursor {
            uidvalidity: g(keys::LAST_UIDVALIDITY).parse().unwrap_or(0),
            uid: g(keys::LAST_UID).parse().unwrap_or(0),
        };
        let fetch_state = std::sync::Arc::clone(&self.fetch_state);
        let db_path = self.data_dir.join("mailbox.db");
        let data_dir = self.data_dir.clone();
        std::thread::spawn(move || {
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

    /// 收件状态 CAS：空闲→进行中（成功 true；已在收件返回 false）
    /// 开始新轮（CAS）：成功返回该轮 epoch（finish 写状态的凭证）
    fn fetch_cas_begin(&self) -> Option<u64> {
        let mut s = self.fetch_state.lock().expect("收件状态锁中毒");
        if s.running {
            return None;
        }
        s.running = true;
        s.just_finished = false;
        s.epoch += 1;
        s.started_at = crate::mail::now_epoch();
        s.last_error.clear();
        Some(s.epoch)
    }

    /// 收件结束（凭据缺失等同步失败路径；线程路径在闭包内直接更新 Arc）
    /// 同步失败收尾（start_fetch 线程前路径）。带 epoch 凭证校验
    fn fetch_end(&self, epoch: u64, new_count: i64, error: &str) {
        let mut s = self.fetch_state.lock().expect("收件状态锁中毒");
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
    /// worker 持独立连接与配置镜像 Arc（配置源 mailbox.toml，不占库锁）
    pub fn start_send_worker(&self) {
        let mut guard = self.send_tx.lock().expect("发送队列锁中毒");
        if guard.is_some() {
            return;
        }
        let Ok(conn) = crate::db::open(&self.data_dir.join("mailbox.db")) else {
            return;
        };
        let config_arc = std::sync::Arc::clone(&self.config);
        let (tx, rx) = std::sync::mpsc::channel::<i64>();
        // 崩溃恢复：残留 queued 补发（按 id 升序）
        let stale: Vec<i64> = conn
            .prepare("SELECT id FROM emails WHERE status = 'queued' ORDER BY id")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, i64>(0))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        std::thread::spawn(move || {
            for id in rx {
                // 发送参数快照：每封派发前重读配置（mailbox.toml；改动即时生效，
                // 不落库——验收拍板简化）
                let smtp = {
                    let cfg = config_arc.lock().expect("配置锁中毒");
                    crate::mail::SmtpConfig::from_config(&cfg)
                };
                // 派发内部所有终态（含各类失败）均已落库；此处 Err 仅剩
                // "抢占失败/读库异常"两条管理性路径，记日志供排查即可
                if let Err(err) =
                    crate::mail::dispatch_one(&conn, &crate::mail::RealSender, id, &smtp)
                {
                    eprintln!("[send] 邮件 {id} 派发未执行：{err}");
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
        if let Some(tx) = self.send_tx.lock().expect("发送队列锁中毒").as_ref() {
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
        let mut conn = self.db.lock().expect("数据库锁中毒");
        f(&mut conn)
    }

    /// 配置镜像快照（外部线程/测试读 mailbox.toml 内存镜像用；worker 线程直接持 Arc）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn config_snapshot(&self) -> crate::conf::ConfigMap {
        self.config.lock().expect("配置锁中毒").clone()
    }

    /// 读键。配置键（conf::CONFIG_KEYS：数据目录/服务器/账号）仅从 mailbox.toml
    /// 内存镜像读（文件为唯一持久源，不落库）；其余读库
    pub fn get(&self, key: &str) -> Option<Option<String>> {
        if crate::conf::is_config_key(key) {
            let cfg = self.config.lock().expect("配置锁中毒");
            return Some(cfg.get(key).cloned());
        }
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

    /// 批量写。路由拆分（验收拍板简化）：配置键仅写独立配置文件 mailbox.toml
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
                let cfg = self.config.lock().expect("配置锁中毒");
                cfg_pairs
                    .iter()
                    .map(|(k, _)| ((*k).to_string(), cfg.get(*k).cloned()))
                    .collect()
            };
            {
                let mut cfg = self.config.lock().expect("配置锁中毒");
                for (k, v) in &cfg_pairs {
                    cfg.insert((*k).to_string(), (*v).to_string());
                }
            }
            let save_res = {
                let cfg = self.config.lock().expect("配置锁中毒");
                crate::conf::save(&self.anchor_dir, &cfg)
            };
            if let Err(e) = save_res {
                let mut cfg = self.config.lock().expect("配置锁中毒");
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
            let mut conn = self.db.lock().expect("数据库锁中毒");
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
        ["mailbox.db", "mailbox.db-wal"]
            .iter()
            .filter_map(|f| std::fs::metadata(self.data_dir.join(f)).ok())
            .map(|m| m.len())
            .sum()
    }

    /// SQLite 版本串（关于页展示）
    pub fn sqlite_version(&self) -> String {
        let conn = self.db.lock().expect("数据库锁中毒");
        conn.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))
            .unwrap_or_default()
    }

    /// 侧栏文件夹计数（列表屏冗余展示）：单条聚合 SQL 一次扫表（5 个计数）
    pub fn folder_counts(&self) -> HashMap<&'static str, i64> {
        let conn = self.db.lock().expect("数据库锁中毒");
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

    /// 待生效的数据目录（M5 迁移）：读 mailbox.toml 指针；
    /// 重启后 resolve_start_dir 应用指针、其值 == 当前目录，提示自然消失
    pub fn pending_data_dir(&self) -> Option<String> {
        let over = match self.get(keys::DATA_DIR) {
            Some(Some(v)) if !v.trim().is_empty() => v,
            _ => return None,
        };
        let pending = PathBuf::from(&over);
        (pending != self.data_dir && pending.join("mailbox.db").is_file()).then_some(over)
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
/// 目标已存在的同名文件覆盖，缺失目录逐级创建
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("创建目录失败：{e}"))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("读取目录失败：{e}"))? {
        let entry = entry.map_err(|e| format!("遍历失败：{e}"))?;
        let ty = entry
            .file_type()
            .map_err(|e| format!("读取类型失败：{e}"))?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to).map_err(|e| format!("复制文件失败：{e}"))?;
        }
    }
    Ok(())
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
        let mut s = fetch_state.lock().expect("收件状态锁中毒");
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
    // 收件主体 catch_unwind（M5 验收反馈）：imap 3.0.0-alpha 解析、入库等任何 panic
    // 不得中断线程——否则 running 永久卡 true，手动收取按钮 CAS 永远被拒，
    // 用户表现为"无法刷新"。捕获后状态复位并把错误透出给 UI。
    let last_uid = last.uid;
    let last_uidvalidity = last.uidvalidity;
    let outcome: Result<(crate::mail::FetchOutcome, usize), String> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let outcome = crate::mail::MailFetcher::fetch(
                &crate::mail::RealFetcher,
                &host,
                port,
                &account,
                &password,
                first_days,
                last,
            )?;
            // 入库与网络拉取同属关键路径：两者任一 panic 都必须复位状态
            let added = crate::mail::store_incoming_conn(&mut conn, &data_dir, &outcome.mails)?;
            Ok((outcome, added))
        }))
        .unwrap_or_else(|_| {
            eprintln!("[fetch] 收件线程 panic（已捕获，状态复位）");
            Err("收件内部异常终止，请重试收取".into())
        });
    match outcome {
        Ok((outcome, added)) => {
            // 游标推进（M5 修复）：uidvalidity 恒记录；uid 以搜索层最大命中为准，
            // 解析失败/去重跳过的邮件不再阻塞游标（否则永远重试同一封）。
            // 游标单调：SINCE 兜底命中可能低于旧游标，不得回退；
            // uidvalidity 变化时 UID 空间重置，必须无条件采纳新值。
            // 游标三键单事务写入：崩溃窗口内不得出现 uidvalidity 已变而 uid 滞留
            // 旧空间的错配（会导致跨空间跳号漏信）
            let mut push_cursor = || -> rusqlite::Result<()> {
                let tx = conn.transaction()?;
                crate::db::settings::set(
                    &tx,
                    keys::LAST_UIDVALIDITY,
                    &outcome.uidvalidity.to_string(),
                )?;
                if outcome.max_uid > 0
                    && (outcome.uidvalidity != last_uidvalidity || outcome.max_uid > last_uid)
                {
                    crate::db::settings::set(&tx, keys::LAST_UID, &outcome.max_uid.to_string())?;
                }
                crate::db::settings::set(
                    &tx,
                    keys::LAST_SYNC_AT,
                    &crate::mail::now_epoch().to_string(),
                )?;
                tx.commit()
            };
            if let Err(err) = push_cursor() {
                eprintln!("[fetch] 游标写入失败（本轮邮件已入库，下轮重拉去重兜底）：{err}");
            }
            eprintln!(
                "[fetch] 完成：命中游标 uid={}，解析 {} 封，入库 {added} 封",
                outcome.max_uid,
                outcome.mails.len()
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
