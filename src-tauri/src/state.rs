//! 应用状态：SQLite 单连接（`Mutex` 内部可变性）+ 业务配置读写
//!
//! 设计 §2/§13-R6：全库走单一 `Mutex<Connection>`，WAL 模式读写不互阻。
//! 账号密码不落库（经 `secrets` 走 OS keychain，keyfile 回退，设计 §5.4）。

use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 数据目录指针文件名（锚点目录下，与锚点库 data_dir_override 双保险）。
/// 纯文本单行路径，迁移时与库指针同步写入：锚点 mailbox.db 被手动删除后
/// （验收反馈：删除后启动误判首次启动）启动解析仍有兑底定位源。
const DATA_DIR_POINTER: &str = "data-dir.txt";

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

    /// 自定义数据目录指针（存于 anchor 库即系统默认目录；M5 迁移功能）
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
    /// 锚点目录（系统默认目录；自定义目录指针 data_dir_override 存于此处的库，
    /// 启动时 resolve_start_dir 先开锚点库读指针再定位实际目录，保证指针永远可寻址）
    anchor_dir: PathBuf,
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

    /// 打开/初始化数据库（lib.rs 专用：锚点目录存自定义目录指针，见 resolve_start_dir）
    pub fn open_with_anchor(data_dir: &Path, anchor_dir: &Path) -> Self {
        std::fs::create_dir_all(data_dir).expect("数据目录应可创建");
        let conn = crate::db::open(&data_dir.join("mailbox.db")).expect("数据库应可打开");
        Self {
            db: Mutex::new(conn),
            data_dir: data_dir.to_path_buf(),
            anchor_dir: anchor_dir.to_path_buf(),
            send_tx: Mutex::new(None),
            fetch_state: std::sync::Arc::new(Mutex::new(FetchState {
                running: false,
                new_count: 0,
                last_error: String::new(),
                just_finished: false,
            })),
        }
    }

    /// 启动目录解析（lib.rs setup 调用）：双源指针定位自定义数据目录，
    /// 均无效/失效则回退锚点目录。
    /// 1) 锚点库 data_dir_override（存在才打开，探测不副作用建库）；
    /// 2) 锚点目录指针文件 data-dir.txt（锚点库被删除后的兑底，验收反馈）。
    ///
    /// 有效性 = 目录存在且含 mailbox.db（迁移快照必有；空壳目录不采纳，回退锚点
    /// 可吃到"原目录保留为备份"的备份）；指针目录消失（外置盘拔出）同样安全回退
    pub fn resolve_start_dir(anchor: &Path) -> PathBuf {
        let usable = |p: &str| Path::new(p).join("mailbox.db").is_file();
        let db_probe = anchor.join("mailbox.db");
        if db_probe.is_file() {
            let over = crate::db::open(&db_probe)
                .ok()
                .and_then(|c| crate::db::settings::get(&c, keys::DATA_DIR).ok().flatten());
            if let Some(p) = over.filter(|p| usable(p)) {
                return PathBuf::from(p);
            }
        }
        if let Some(p) = read_pointer_file(anchor).filter(|p| usable(p)) {
            return PathBuf::from(p);
        }
        anchor.to_path_buf()
    }

    /// 迁移数据目录到用户选定位置（M5：在线快照 + 重启生效，避免热切换连接的并发风险）。
    /// 成功后返回用户提示文案；原目录保留为备份（用户确认无误后可手动删除）。
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

        // 3) 指针写入锚点库（启动 resolve_start_dir 读取处；锚点==当前目录时写当前库）
        let anchor_db = self.anchor_dir.join("mailbox.db");
        let anchor_conn =
            crate::db::open(&anchor_db).map_err(|e| format!("锚点库打开失败：{e}"))?;
        crate::db::settings::set(&anchor_conn, keys::DATA_DIR, &dest_path.to_string_lossy())
            .map_err(|e| format!("指针写入失败：{e}"))?;
        // 指针文件双保险：锚点 mailbox.db 被手动删除时（验收反馈）启动仍有定位源；
        // 锚点库刚写入成功说明目录可写，此处失败应视为迁移不完整
        std::fs::write(
            self.anchor_dir.join(DATA_DIR_POINTER),
            dest_path.to_string_lossy().as_bytes(),
        )
        .map_err(|e| format!("指针文件写入失败：{e}"))?;

        Ok(format!(
            "数据已迁移到新目录，重启应用后生效；原目录保留为备份（请勿删除其中的 mailbox.db 与 data-dir.txt）：{}",
            cur.display()
        ))
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

    /// 待生效的数据目录（M5 迁移）：读锚点库的 data_dir_override 指针；
    /// 重启后 resolve_start_dir 应用指针、其值 == 当前目录，提示自然消失
    pub fn pending_data_dir(&self) -> Option<String> {
        let conn = crate::db::open(&self.anchor_dir.join("mailbox.db")).ok()?;
        let over = crate::db::settings::get(&conn, keys::DATA_DIR)
            .ok()
            .flatten()?;
        let pending = PathBuf::from(&over);
        if pending != self.data_dir && pending.is_dir() {
            Some(over)
        } else {
            None
        }
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

/// 读锚点目录的指针文件（迁移时与库指针同步写入；文件缺失/空/读失败 → None，
/// 读取失败按无指针处理、安全回退锚点目录）
fn read_pointer_file(anchor: &Path) -> Option<String> {
    let text = std::fs::read_to_string(anchor.join(DATA_DIR_POINTER)).ok()?;
    let p = text.trim();
    (!p.is_empty()).then(|| p.to_string())
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
        assert_eq!(ptr, dest.to_string_lossy(), "指针应写锚点库");
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
        // 失败不应留下指针
        let conn = crate::db::open(&cur.join("mailbox.db")).unwrap();
        assert!(
            crate::db::settings::get(&conn, keys::DATA_DIR)
                .unwrap()
                .is_none(),
            "失败不得写指针"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_start_dir_falls_back_safely() {
        // 无库 / 无指针 → 回退锚点
        let empty = temp_dir("resolve-empty");
        assert_eq!(AppState::resolve_start_dir(&empty), empty);
        // 指针目录不存在（外置盘拔出）→ 回退锚点
        let anchor = temp_dir("resolve-anchor");
        let state = AppState::open(&anchor);
        state
            .set_batch(&[(keys::DATA_DIR, "/nonexistent/removed-usb")])
            .unwrap();
        drop(state);
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
        // 验收反馈场景：迁移后用户删除锚点目录 mailbox.db —— 若仅依赖库指针，
        // 启动会在锚点目录新建空库并误判首次启动；指针文件双保险应继续定位自定义目录
        let base = temp_dir("resolve-anchor-del");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let state = AppState::open(&anchor);
        // 造一封邮件：验证经指针文件定位后数据完整（非空壳）
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
        // 双指针就位：锚点库 + data-dir.txt
        assert!(anchor.join("mailbox.db").is_file(), "锚点库存在");
        assert!(
            anchor.join(DATA_DIR_POINTER).is_file(),
            "指针文件已同步写入"
        );
        // 用户删除锚点库（保留指针文件的文件名无关性）
        std::fs::remove_file(anchor.join("mailbox.db")).unwrap();
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            dest,
            "锚点库被删后应经指针文件继续定位自定义目录（不误判首次启动）"
        );
        // 定位到的库可正常打开且数据完整（非空壳）
        let conn = crate::db::open(&dest.join("mailbox.db")).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "自定义目录数据完整");
        // 指针文件也失效（指向已删目录）→ 回退锚点
        std::fs::write(anchor.join(DATA_DIR_POINTER), "/nonexistent/usb-gone").unwrap();
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            anchor,
            "双指针均失效应回退锚点"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_start_dir_rejects_hollow_pointer_target() {
        // 有效性加强：指针指向的目录存在但无 mailbox.db（空壳目录）→ 不采纳，回退锚点
        let base = temp_dir("resolve-hollow");
        let anchor = base.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        let hollow = base.join("hollow");
        std::fs::create_dir_all(&hollow).unwrap();
        std::fs::write(anchor.join(DATA_DIR_POINTER), hollow.to_str().unwrap()).unwrap();
        assert_eq!(
            AppState::resolve_start_dir(&anchor),
            anchor,
            "空壳目录不得被当作有效数据目录"
        );
        let _ = std::fs::remove_dir_all(&base);
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
