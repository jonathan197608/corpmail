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

    /// 自定义数据目录指针（存独立配置文件 mailbox.conf，随锚点目录常驻）
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
    /// 锚点目录（系统默认应用数据目录）：独立配置文件 mailbox.conf 在此，
    /// 不随数据目录迁移、不被备份删除影响（resolve_start_dir 亦从此定位）
    anchor_dir: PathBuf,
    /// 应用配置内存镜像（mailbox.conf：数据目录指针 + 服务器 + 账号；
    /// 读经 get 路由，写经 set_batch 双写文件与库镜像）
    config: Mutex<crate::conf::ConfigMap>,
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
    /// 打开/初始化数据库（schema 幂等迁移在 db::open 内完成；锚点=数据目录本身，
    /// 适用于测试与未启用自定义目录的场景。生产入口为 open_with_anchor）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(data_dir: &Path) -> Self {
        Self::open_with_anchor(data_dir, data_dir)
    }

    /// 打开/初始化数据库（lib.rs 专用：锚点目录存独立配置文件 mailbox.conf）。
    /// 打开后同步配置：文件 ↔ 库镜像并集（冲突以库为准——库为活跃写入侧；
    /// 0.2.2 旧版升级首启时库键回填到文件，文件从备份恢复时回填入库）
    pub fn open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Self {
        std::fs::create_dir_all(data_dir).expect("数据目录应可创建");
        let conn = crate::db::open(&data_dir.join("mailbox.db")).expect("数据库应可打开");
        let config = Self::load_and_sync_config(&conn, anchor_dir);
        Self {
            db: Mutex::new(conn),
            data_dir: data_dir.to_path_buf(),
            anchor_dir: anchor_dir.to_path_buf(),
            config: Mutex::new(config),
            send_tx: Mutex::new(None),
            fetch_state: std::sync::Arc::new(Mutex::new(FetchState {
                running: false,
                new_count: 0,
                last_error: String::new(),
                just_finished: false,
            })),
        }
    }

    /// 配置启动同步：mailbox.conf ↔ 活跃库镜像并集，任一份缺失的键由另一份回填。
    /// - 冲突以库为准（库是每次保存的活跃写入侧；文件写失败/落后时以库覆盖）；
    /// - DATA_DIR 额外回填源：锚点库（0.2.1/0.2.2 迁移指针写在锚点库，活跃库无）；
    /// - 回填结果双写落盘；文件落盘失败仅记日志（库镜像仍在，下次启动重试）
    fn load_and_sync_config(conn: &Connection, anchor: &Path) -> crate::conf::ConfigMap {
        let mut cfg = crate::conf::load(anchor);
        let mut file_dirty = false;
        let mut db_missing: Vec<(String, String)> = Vec::new();
        for k in crate::conf::CONFIG_KEYS {
            let db_v = crate::db::settings::get(conn, k).ok().flatten();
            match (cfg.get(*k).cloned(), db_v) {
                (Some(f), Some(dv)) => {
                    if f != dv {
                        cfg.insert((*k).to_string(), dv);
                        file_dirty = true;
                    }
                }
                (None, Some(dv)) => {
                    cfg.insert((*k).to_string(), dv);
                    file_dirty = true;
                }
                (Some(fv), None) => db_missing.push(((*k).to_string(), fv)),
                (None, None) => {}
            }
        }
        if !cfg.contains_key(keys::DATA_DIR) {
            let anchor_db = anchor.join("mailbox.db");
            if anchor_db.is_file()
                && let Ok(c) = crate::db::open(&anchor_db)
                && let Ok(Some(p)) = crate::db::settings::get(&c, keys::DATA_DIR)
                && !p.trim().is_empty()
            {
                db_missing.push((keys::DATA_DIR.to_string(), p.clone()));
                cfg.insert(keys::DATA_DIR.to_string(), p);
                file_dirty = true;
            }
        }
        for (k, v) in &db_missing {
            let _ = crate::db::settings::set(conn, k, v);
        }
        if file_dirty && let Err(e) = crate::conf::save(anchor, &cfg) {
            eprintln!("[conf] 配置文件写入失败（库镜像仍有效）：{e}");
        }
        cfg
    }

    /// 启动目录解析（lib.rs setup 调用）：独立配置文件 mailbox.conf 的 data_dir_override
    /// 指针定位自定义数据目录；文件缺失（0.2.1/0.2.2 升级首启）回退锚点库指针。
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
        // 兼容 0.2.1/0.2.2：迁移指针在锚点库（仅打开既有文件，探测无副作用建库）
        let anchor_db = anchor.join("mailbox.db");
        if anchor_db.is_file() {
            let over = crate::db::open(&anchor_db)
                .ok()
                .and_then(|c| crate::db::settings::get(&c, keys::DATA_DIR).ok().flatten());
            if let Some(p) = over.filter(|p| usable(p)) {
                return PathBuf::from(p);
            }
        }
        anchor.to_path_buf()
    }

    /// 迁移数据目录到用户选定位置（M5：在线快照 + 重启生效，避免热切换连接的并发风险）。
    /// 指针写入独立配置文件 mailbox.conf（随锚点目录常驻，不随数据迁移、
    /// 不被备份删除影响）；锚点库同步兼容镜像。原目录残留备份可由用户在
    /// 存储页手动删除（delete_old_backup）。
    /// 迁移期间收件线程可能写入新附件导致极小竞态窗口（附件文件缺失但元数据在），
    /// 重启后孤儿回收不会破坏元数据，重收件可补——概率极低，取舍见设计 §5.2 注记
    pub fn move_data_dir(&self, dest: &str) -> Result<String, String> {
        let dest_path = PathBuf::from(dest.trim_end_matches(['/', '\\']));
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

        // 3) 指针写入独立配置文件（mailbox.conf）+ 锚点库兼容镜像。
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
        let anchor_db = self.anchor_dir.join("mailbox.db");
        if anchor_db.is_file()
            && let Ok(c) = crate::db::open(&anchor_db)
        {
            let _ = crate::db::settings::set(&c, keys::DATA_DIR, &dest_path.to_string_lossy());
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
    /// 与 attachments/；配置文件 mailbox.conf 与当前数据不受影响。
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
        self.fetch_state.lock().expect("收件状态锁中毒").clone()
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
        if !self.fetch_cas_begin() {
            return; // 已在收件中（防抖 §6.2）
        }
        let d = defaults();
        let g = |k: &str| self.get_or(k, &d).unwrap_or_default();
        let host = g(keys::RECV_HOST);
        let port: u16 = g(keys::RECV_PORT).parse().unwrap_or(993);
        let account = full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN));
        let password = match self.retrieve_password(&account) {
            crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
            crate::secrets::GetResult::Missing => {
                self.fetch_end(0, "凭据缺失，请先在设置中保存密码");
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
            let finish = |new_count: i64, error: String| {
                let mut s = fetch_state.lock().expect("收件状态锁中毒");
                s.running = false;
                s.new_count = new_count;
                s.last_error = error;
                s.just_finished = true;
            };
            let Ok(mut conn) = crate::db::open(&db_path) else {
                finish(0, "本地数据库打开失败".into());
                return;
            };
            // 收件主体 catch_unwind（M5 验收反馈）：imap 3.0.0-alpha 解析等任何 panic
            // 不得中断线程——否则 running 永久卡 true，手动收取按钮 CAS 永远被拒，
            // 用户表现为"无法刷新"。捕获后状态复位并把错误透出给 UI。
            let outcome: Result<crate::mail::FetchOutcome, String> =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::mail::MailFetcher::fetch(
                        &crate::mail::RealFetcher,
                        &host,
                        port,
                        &account,
                        &password,
                        first_days,
                        last,
                    )
                })) {
                    Ok(r) => r,
                    Err(_) => {
                        eprintln!("[fetch] 收件线程 panic（已捕获，状态复位）");
                        Err("收件内部异常终止，请重试收取".into())
                    }
                };
            match outcome {
                Ok(outcome) => {
                    let added = match crate::mail::store_incoming_conn(
                        &mut conn,
                        &data_dir,
                        &outcome.mails,
                    ) {
                        Ok(n) => n,
                        Err(err) => {
                            eprintln!("[fetch] 入库失败：{err}");
                            0
                        }
                    };
                    // 游标推进（M5 修复）：uidvalidity 恒记录；uid 以搜索层最大命中为准，
                    // 解析失败/去重跳过的邮件不再阻塞游标（否则永远重试同一封）
                    let set = |k: &str, v: &str| {
                        let _ = crate::db::settings::set(&conn, k, v);
                    };
                    set(keys::LAST_UIDVALIDITY, &outcome.uidvalidity.to_string());
                    if outcome.max_uid > 0 {
                        set(keys::LAST_UID, &outcome.max_uid.to_string());
                    }
                    set(keys::LAST_SYNC_AT, &crate::mail::now_epoch().to_string());
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
        });
    }

    /// 收件状态 CAS：空闲→进行中（成功 true；已在收件返回 false）
    fn fetch_cas_begin(&self) -> bool {
        let mut s = self.fetch_state.lock().expect("收件状态锁中毒");
        if s.running {
            return false;
        }
        s.running = true;
        s.just_finished = false;
        true
    }

    /// 收件结束（凭据缺失等同步失败路径；线程路径在闭包内直接更新 Arc）
    fn fetch_end(&self, new_count: i64, error: &str) {
        let mut s = self.fetch_state.lock().expect("收件状态锁中毒");
        s.running = false;
        s.new_count = new_count;
        s.last_error = error.into();
        s.just_finished = true;
    }

    /// 启动发送 worker（M3，设计 §2 单飞线程；生产装配于 lib.rs，测试不启动）。
    ///
    /// 独立连接 + MPSC 队列逐封派发（企业 SMTP 串行更稳）；
    /// 启动时扫描残留 queued 一并补发（崩溃恢复）。
    pub fn start_send_worker(&self) {
        let mut guard = self.send_tx.lock().expect("发送队列锁中毒");
        if guard.is_some() {
            return;
        }
        let Ok(conn) = crate::db::open(&self.data_dir.join("mailbox.db")) else {
            return;
        };
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
                let _ = crate::mail::dispatch_one(&conn, &crate::mail::RealSender, id);
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
    pub fn with_conn<R>(&self, f: impl FnOnce(&mut rusqlite::Connection) -> R) -> R {
        let mut conn = self.db.lock().expect("数据库锁中毒");
        f(&mut conn)
    }

    /// 读 settings 键（None = 未设置；库错误按未设置处理，页面回退默认值）
    /// 读键。配置键（conf::CONFIG_KEYS：数据目录/服务器/账号）路由到
    /// mailbox.conf 内存镜像（缺失时兜底库镜像——启动并集同步后罕见）；其余读库
    pub fn get(&self, key: &str) -> Option<Option<String>> {
        if crate::conf::is_config_key(key) {
            let cfg = self.config.lock().expect("配置锁中毒");
            if let Some(v) = cfg.get(key) {
                return Some(Some(v.clone()));
            }
            drop(cfg);
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

    /// 批量写（单事务；单键写入同样走此接口，保持全库单一写入口）。
    /// 配置键双写：先库镜像后独立配置文件（文件写失败时库已更新且冲突策略
    /// 以库为准——重启并集同步后自愈）；注意错误类型为 String
    pub fn set_batch(&self, pairs: &[(&str, &str)]) -> Result<(), String> {
        let has_cfg = pairs.iter().any(|(k, _)| crate::conf::is_config_key(k));
        if has_cfg {
            let mut cfg = self.config.lock().expect("配置锁中毒");
            for (k, v) in pairs {
                if crate::conf::is_config_key(k) {
                    cfg.insert((*k).to_string(), (*v).to_string());
                }
            }
        }
        {
            let mut conn = self.db.lock().expect("数据库锁中毒");
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            for (k, v) in pairs {
                tx.execute(
                    "INSERT INTO settings(k, v) VALUES (?1, ?2)
                     ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                    [k, v],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
        }
        if has_cfg {
            let cfg = self.config.lock().expect("配置锁中毒");
            crate::conf::save(&self.anchor_dir, &cfg)
                .map_err(|e| format!("配置文件写入失败：{e}"))?;
        }
        Ok(())
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

    /// 待生效的数据目录（M5 迁移）：读配置指针（get 路由 mailbox.conf/库镜像）；
    /// 重启后 resolve_start_dir 应用指针、其值 == 当前目录，提示自然消失
    pub fn pending_data_dir(&self) -> Option<String> {
        let over = match self.get(keys::DATA_DIR) {
            Some(Some(v)) if !v.trim().is_empty() => v,
            _ => return None,
        };
        let pending = PathBuf::from(&over);
        (pending != self.data_dir && pending.join("mailbox.db").is_file()).then_some(over)
    }

    /// 数据目录下的独立附件占用（M1 尚无附件；目录不存在 → 0）：(字节, 文件数)
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
    fn move_data_dir_snapshots_and_writes_pointer() {
        let base = temp_dir("move-ok");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let state = AppState::open(&anchor);
        // 造数据：配置 + 一封草稿 + 一个附件独立文件
        state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
        let id = crate::mail::save_draft(
            &state,
            &crate::mail::DraftInput {
                id: None,
                from_addr: "demo@chinatelecom.cn".into(),
                from_name: "张伟".into(),
                to_addrs: "peer@x.cn".into(),
                cc_addrs: String::new(),
                subject: "迁移用例".into(),
                body_text: "正文".into(),
                in_reply_to: None,
            },
        )
        .unwrap();
        let att = anchor.join("attachments/2026-10").join(id.to_string());
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(att.join("1_a.bin.gz"), b"gz-bytes").unwrap();

        let dest = base.join("moved");
        let msg = state.move_data_dir(dest.to_str().unwrap()).unwrap();
        assert!(msg.contains("重启"), "提示应含重启生效：{msg}");
        // 新库可打开且数据齐全
        let moved_db = dest.join("mailbox.db");
        let conn = crate::db::open(&moved_db).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "快照应含全部邮件");
        let prefix = crate::db::settings::get(&conn, keys::ACCOUNT_PREFIX)
            .unwrap()
            .unwrap_or_default();
        assert_eq!(prefix, "demo", "设置随库迁移");
        assert!(
            dest.join(format!("attachments/2026-10/{id}/1_a.bin.gz"))
                .exists(),
            "附件文件应复制"
        );
        // 锚点库写入指针；启动解析定位新目录
        let anchor_conn = crate::db::open(&anchor.join("mailbox.db")).unwrap();
        let ptr = crate::db::settings::get(&anchor_conn, keys::DATA_DIR)
            .unwrap()
            .unwrap_or_default();
        assert_eq!(ptr, dest.to_string_lossy(), "兼容镜像应写锚点库");
        // 独立配置文件为主指针
        let cfg = crate::conf::load(&anchor);
        assert_eq!(
            cfg.get(keys::DATA_DIR).map(String::as_str),
            Some(dest.to_str().unwrap()),
            "指针应写独立配置文件 mailbox.conf"
        );
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            dest,
            "启动解析应定位新目录"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_data_dir_rejects_invalid_targets() {
        let base = temp_dir("move-bad");
        let cur = base.join("cur");
        std::fs::create_dir_all(&cur).unwrap();
        let state = AppState::open(&cur);
        assert!(
            state.move_data_dir(cur.to_str().unwrap()).is_err(),
            "相同目录拒绝"
        );
        assert!(
            state
                .move_data_dir(cur.join("sub").to_str().unwrap())
                .is_err(),
            "子目录拒绝"
        );
        let other = base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        // 已有 mailbox.db 的目录拒绝
        std::fs::write(other.join("mailbox.db"), b"exists").unwrap();
        assert!(
            state.move_data_dir(other.to_str().unwrap()).is_err(),
            "占用目录拒绝"
        );
        // 相对路径拒绝
        assert!(state.move_data_dir("relative/dir").is_err());
        // 失败不应留下指针（配置文件与库镜像均不得写入）
        let conn = crate::db::open(&cur.join("mailbox.db")).unwrap();
        assert!(
            crate::db::settings::get(&conn, keys::DATA_DIR)
                .unwrap()
                .is_none(),
            "失败不得写库指针"
        );
        drop(conn);
        assert!(
            !crate::conf::load(&cur).contains_key(keys::DATA_DIR),
            "失败不得写配置文件指针"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_start_dir_falls_back_safely() {
        // 无库 / 无配置 → 回退锚点
        let empty = temp_dir("resolve-empty");
        assert_eq!(AppState::resolve_start_dir(&empty), empty);
        // 配置指针指向不存在的目录（外置盘拔出）→ 回退锚点
        let anchor = temp_dir("resolve-anchor");
        let state = AppState::open(&anchor);
        state
            .set_batch(&[(keys::DATA_DIR, "/nonexistent/removed-usb")])
            .unwrap();
        drop(state);
        assert!(
            anchor.join(crate::conf::CONFIG_FILE).is_file(),
            "set_batch 应同步写独立配置文件"
        );
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            anchor,
            "指针失效应回退锚点目录"
        );
        let _ = std::fs::remove_dir_all(&empty);
        let _ = std::fs::remove_dir_all(&anchor);
    }

    #[test]
    fn resolve_start_dir_survives_anchor_db_deletion() {
        // 验收反馈场景：迁移后用户删除锚点目录 mailbox.db —— 指针在独立配置文件
        // mailbox.conf 中，删除库不影响定位，不误判首次启动
        let base = temp_dir("resolve-anchor-del");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let state = AppState::open(&anchor);
        // 造一封邮件：验证定位后数据完整（非空壳）
        state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
        crate::mail::save_draft(
            &state,
            &crate::mail::DraftInput {
                id: None,
                from_addr: "demo@chinatelecom.cn".into(),
                from_name: "张伟".into(),
                to_addrs: "peer@x.cn".into(),
                cc_addrs: String::new(),
                subject: "兜底定位用例".into(),
                body_text: "正文".into(),
                in_reply_to: None,
            },
        )
        .unwrap();
        let dest = base.join("moved");
        state.move_data_dir(dest.to_str().unwrap()).unwrap();
        drop(state);
        // 迁移后：锚点保留兼容镜像库 + 独立配置文件（配置不随数据迁移）
        assert!(anchor.join("mailbox.db").is_file(), "锚点兼容镜像库存在");
        assert!(
            anchor.join(crate::conf::CONFIG_FILE).is_file(),
            "独立配置文件已写入"
        );
        // 用户删除锚点库 —— 定位不受影响
        std::fs::remove_file(anchor.join("mailbox.db")).unwrap();
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            dest,
            "锚点库被删后应经配置文件继续定位自定义目录（不误判首次启动）"
        );
        // 定位到的库可正常打开且数据完整（非空壳）
        let conn = crate::db::open(&dest.join("mailbox.db")).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "自定义目录数据完整");
        // 模拟重启后（数据目录已在新位置）：不复活锚点库、无待生效提示
        let state2 = AppState::open_with_anchor(&dest, &anchor);
        assert!(
            !anchor.join("mailbox.db").exists(),
            "open 不得复活锚点库文件"
        );
        assert_eq!(
            state2.pending_data_dir(),
            None,
            "指针已生效（==当前目录）应无待生效提示"
        );
        // 配置指针也失效（指向已删目录）→ 回退锚点
        let mut cfg = crate::conf::load(&anchor);
        cfg.insert(keys::DATA_DIR.to_string(), "/nonexistent/usb-gone".into());
        crate::conf::save(&anchor, &cfg).unwrap();
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            anchor,
            "指针失效应回退锚点"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pending_data_dir_reads_config_file_after_db_delete() {
        // 未重启状态下删锚点库：待生效目录从 mailbox.conf 读出（迁移待生效仍可见）
        let base = temp_dir("pending-conf");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let state = AppState::open(&anchor);
        let dest = base.join("moved");
        state.move_data_dir(dest.to_str().unwrap()).unwrap();
        drop(state);
        std::fs::remove_file(anchor.join("mailbox.db")).unwrap();
        // 重新打开（锚点=默认目录，数据目录也还是它——模拟未重启进程）
        let state2 = AppState::open_with_anchor(&anchor, &anchor);
        assert_eq!(
            state2.pending_data_dir().as_deref(),
            Some(dest.to_str().unwrap()),
            "迁移待生效应能从配置文件读出"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_start_dir_rejects_hollow_pointer_target() {
        // 有效性加强：配置指针指向存在但无 mailbox.db 的目录（空壳）→ 不采纳，回退锚点
        let base = temp_dir("resolve-hollow");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let hollow = base.join("hollow");
        std::fs::create_dir_all(&hollow).unwrap();
        let mut cfg = crate::conf::ConfigMap::new();
        cfg.insert(
            keys::DATA_DIR.to_string(),
            hollow.to_string_lossy().into_owned(),
        );
        crate::conf::save(&anchor, &cfg).unwrap();
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            anchor,
            "空壳目录不得被当作有效数据目录"
        );
        // 0.2.1/0.2.2 兼容：锚点库 data_dir_override 同样校验
        let legacy = base.join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        {
            let c = crate::db::open(&legacy.join("mailbox.db")).unwrap();
            crate::db::settings::set(&c, keys::DATA_DIR, hollow.to_str().unwrap()).unwrap();
        }
        assert_eq!(
            AppState::resolve_start_dir(&legacy),
            legacy,
            "旧版库指针指向空壳同样回退"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn config_keys_dual_write_and_survive_file_loss() {
        // 配置键双写：mailbox.conf + 库镜像；get 经路由读到
        let base = temp_dir("conf-dual");
        let dir = base.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::open(&dir);
        state
            .set_batch(&[
                (keys::ACCOUNT_PREFIX, "zhangw.gd"),
                (keys::SMTP_HOST, "smtp.test.cn"),
                (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
                (keys::KEEP_INBOX_DAYS, "60"), // 非配置键：仅入库
            ])
            .unwrap();
        assert_eq!(
            state.get(keys::SMTP_HOST).unwrap().as_deref(),
            Some("smtp.test.cn")
        );
        let cfg = crate::conf::load(&dir);
        assert_eq!(
            cfg.get(keys::SMTP_HOST).map(String::as_str),
            Some("smtp.test.cn"),
            "配置文件应含服务器配置"
        );
        assert_eq!(
            cfg.get(keys::ACCOUNT_PREFIX).map(String::as_str),
            Some("zhangw.gd"),
            "配置文件应含账号配置"
        );
        assert!(
            !cfg.contains_key(keys::KEEP_INBOX_DAYS),
            "非配置键不得入配置文件"
        );
        // 库镜像存在（直接 db 层可读）
        {
            let conn = crate::db::open(&dir.join("mailbox.db")).unwrap();
            assert_eq!(
                crate::db::settings::get(&conn, keys::SMTP_HOST)
                    .unwrap()
                    .as_deref(),
                Some("smtp.test.cn")
            );
        }
        drop(state);
        // 配置文件丢失 → 从库镜像自愈
        std::fs::remove_file(dir.join(crate::conf::CONFIG_FILE)).unwrap();
        let state2 = AppState::open(&dir);
        assert_eq!(
            state2.get(keys::SMTP_HOST).unwrap().as_deref(),
            Some("smtp.test.cn"),
            "配置文件丢失后应从库镜像回填"
        );
        assert!(
            dir.join(crate::conf::CONFIG_FILE).is_file(),
            "自愈应重建配置文件"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_db_settings_backfill_into_config_file() {
        // 0.2.2 升级场景：账号/服务器配置与迁移指针都在库里，无 mailbox.conf ——
        // 首次启动应整体回填到独立配置文件，后续读写以文件为准
        let base = temp_dir("conf-legacy");
        let anchor = base.join("anchor");
        let custom = base.join("custom");
        std::fs::create_dir_all(&anchor).unwrap();
        std::fs::create_dir_all(&custom).unwrap();
        // 自定义库（0.2.2 迁移产物）：账号/服务器配置
        {
            let c = crate::db::open(&custom.join("mailbox.db")).unwrap();
            crate::db::settings::set(&c, keys::ACCOUNT_PREFIX, "zhangw.gd").unwrap();
            crate::db::settings::set(&c, keys::DISPLAY_NAME, "张伟").unwrap();
            crate::db::settings::set(&c, keys::RECV_HOST, "imap.test.cn").unwrap();
        }
        // 锚点库：0.2.2 迁移指针（活跃库不含此键）
        {
            let c = crate::db::open(&anchor.join("mailbox.db")).unwrap();
            crate::db::settings::set(&c, keys::DATA_DIR, custom.to_str().unwrap()).unwrap();
        }
        // 启动：resolve 走锚点库兼容路径定位自定义目录
        assert_eq!(AppState::resolve_start_dir(&anchor), custom);
        let state = AppState::open_with_anchor(&custom, &anchor);
        // 配置文件已回填（含锚点库指针）
        let cfg = crate::conf::load(&anchor);
        assert_eq!(
            cfg.get(keys::ACCOUNT_PREFIX).map(String::as_str),
            Some("zhangw.gd")
        );
        assert_eq!(
            cfg.get(keys::DISPLAY_NAME).map(String::as_str),
            Some("张伟")
        );
        assert_eq!(
            cfg.get(keys::RECV_HOST).map(String::as_str),
            Some("imap.test.cn")
        );
        assert_eq!(
            cfg.get(keys::DATA_DIR).map(String::as_str),
            Some(custom.to_str().unwrap()),
            "锚点库指针应回填到配置文件"
        );
        // 下次启动走配置文件路径（锚点库删掉也能定位）
        std::fs::remove_file(anchor.join("mailbox.db")).unwrap();
        assert_eq!(AppState::resolve_start_dir(&anchor), custom);
        assert!(state.configured(), "回填后应判定已配置");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn delete_old_backup_removes_files_but_keeps_config() {
        // 迁移后（重启生效态）：删除锚点备份 —— 数据/定位不受影响
        let base = temp_dir("backup-del");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let state = AppState::open(&anchor);
        state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
        let att = anchor.join("attachments/2026-10/1");
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(att.join("1_a.bin.gz"), [7u8; 64]).unwrap();
        let dest = base.join("moved");
        state.move_data_dir(dest.to_str().unwrap()).unwrap();
        drop(state);
        // 模拟重启：数据目录已在新位置
        let state2 = AppState::open_with_anchor(&dest, &anchor);
        let (bytes, files) = state2.old_backup_stats().expect("应统计到备份");
        assert!(bytes >= 64, "统计应含附件文件：{bytes}");
        assert!(files >= 2, "统计应含库与附件文件：{files}");
        let (freed, removed) = state2.delete_old_backup().unwrap();
        assert_eq!(freed, bytes, "释放字节应与统计一致");
        assert_eq!(removed, files);
        assert!(!anchor.join("mailbox.db").exists(), "锚点库应被删除");
        assert!(!anchor.join("attachments").exists(), "备份附件目录应被删除");
        assert!(
            anchor.join(crate::conf::CONFIG_FILE).is_file(),
            "配置文件必须保留"
        );
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            dest,
            "删除备份后定位不受影响"
        );
        assert!(dest.join("mailbox.db").is_file(), "当前数据完好");
        // 重复删除 → 无备份可删
        assert!(state2.delete_old_backup().is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn delete_old_backup_refuses_without_migration() {
        // 未迁移（数据就在默认目录）：无可删除备份
        let dir = temp_dir("backup-none");
        let state = AppState::open(&dir);
        assert_eq!(
            state.delete_old_backup().unwrap_err(),
            "当前使用默认数据目录，无迁移备份"
        );
        assert!(state.old_backup_stats().is_none());
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
