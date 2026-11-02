//! IMAP 收件子模块（M4，设计 §6.1/§6.2）
//!
//! RealFetcher：连接 → SELECT INBOX → 增量 UID 搜索（uid+1:* 续拉，空命中 SINCE 兜底）
//! → 大小预检 → 分 run FETCH（连续同类 UID 归并，一条命令拉一塊；超限只拉信头）
//! → mailparse 解析（编码修复：latin1/cp1252 误标 mojibake 逆推 + GBK 兜底）。
//! 入库 store_incoming_conn：每封一事务（正文行 + 附件原子提交），remote_uid/message_id 双去重。
//!
//! 收敛：`fetch_batched` 是**唯一**拉取实现，trait 的 `fetch` 只是把批次收集成整包
//! （旧实现两处各写一遍搜索/拉取/解析，任何修复都要改两次，已出现过漂移）。

use super::{
    AppState, AttachCtx, attach_root_of, e, inline_limit_conn, insert_attachment, is_cjk,
    mark_read, month_dir_conn, now_epoch, safe_filename, sync_attach_stats, tokenize, zoned_of,
};
use crate::net::TlsMode;
use crate::state::{defaults, full_address, keys};
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 单批拉取封数上限
const BATCH: usize = 25;

/// 单批累计原始字节上限（超出即提前 flush）：整封 + 附件 + gzip 副本同时驻留内存，
/// 只按封数分批时 25 封 × 25MB 仍可达 600MB 级峰值（审计批 0+2）
const BATCH_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// 大小预检的 UID 分块：一条 FETCH 命令里塞几千个 UID 会撞服务器命令行长度上限
const SIZE_CHECK_CHUNK: usize = 200;

/// 已读回写的 UID 分块（单条 STORE 命令的 UID 集合上限）
const SEEN_CHUNK: usize = 200;

/// UIDVALIDITY 变化时的固定错误前缀（worker 据此决定"清标志放弃"而非重试）
pub const ERR_UIDVALIDITY_CHANGED: &str = "邮箱 UID 已重排";

/// 一次网络往返要拉的连续同类 UID 块（over = 超限只拉信头）
#[derive(Debug, PartialEq, Eq)]
struct FetchRun {
    uids: Vec<u32>,
    over: bool,
}

/// 超限 run 的块大小（只拉 HEADER，代价低，不受字节预算约束）
const OVER_CHUNK: usize = 200;

/// 计划拉取 run（纯函数，单测覆盖）。逐条 FETCH（每封一次往返）在大邮箱首拉时
/// 是万次 RTT 级，这里把**连续同类**的 UID 归并成一个 run、一条命令拉完：
/// - 全局升序不变量（红线）：只做"连续同类归并"，绝不按类别重排。若拆成
///   "先全部正常、再全部超限"两个列表，崩溃时游标已推到大 UID，夹在中间
///   未拉的邮件会被增量续拉永久跳过（静默漏信）
/// - 正常 run 双阈值：≤BATCH 封且已知累计字节 ≤BATCH_MAX_BYTES（与 flush
///   预算同源，单次响应体驻留内存的峰值不劣化）；大小未知时仅按封数兜底
/// - 超限判定完全来自 prefetch_sizes 预检；预检失败 = 全按正常处理（旧行为）
fn plan_runs(uids: &[u32], sizes: &HashMap<u32, u32>, max_mail_bytes: u64) -> Vec<FetchRun> {
    let mut runs: Vec<FetchRun> = Vec::new();
    let mut run_uids: Vec<u32> = Vec::new();
    let mut run_over: Option<bool> = None;
    let mut run_bytes = 0u64;
    for &uid in uids {
        let over = sizes
            .get(&uid)
            .is_some_and(|&s| u64::from(s) > max_mail_bytes);
        let size = sizes.get(&uid).copied().map(u64::from).unwrap_or(0);
        let switch = run_over != Some(over);
        let full = match run_over {
            None => false,
            Some(true) => run_uids.len() >= OVER_CHUNK,
            Some(false) => {
                run_uids.len() >= BATCH || (size > 0 && run_bytes + size > BATCH_MAX_BYTES)
            }
        };
        if run_over.is_some() && (switch || full) {
            runs.push(FetchRun {
                uids: std::mem::take(&mut run_uids),
                over: run_over.unwrap_or(false),
            });
            run_bytes = 0;
        }
        run_over = Some(over);
        run_uids.push(uid);
        if !over {
            run_bytes += size;
        }
    }
    if !run_uids.is_empty() {
        runs.push(FetchRun {
            uids: run_uids,
            over: run_over.unwrap_or(false),
        });
    }
    runs
}

/// 收件附件（内存字节；来自 MIME 解析）
#[derive(Clone)]
pub struct IncomingAttach {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// 收件结果单封形态（fetcher 返回；入库走 store_incoming）
#[derive(Clone)]
pub struct IncomingMail {
    pub uidvalidity: u32,
    pub uid: u32,
    /// 未含 \Seen 则置未读（§6.1）
    pub seen: bool,
    pub internal_date: Option<i64>,
    pub message_id: String,
    pub subject: String,
    pub from_addr: String,
    pub from_name: String,
    pub to_addrs: String,
    /// 抄送地址（逗号分隔纯地址；联系人采集用）
    pub cc_addrs: String,
    pub body_text: String,
    pub body_kind: String,
    pub attachments: Vec<IncomingAttach>,
    /// 超过单封上限：只拉了信头，正文/附件未下载（正文为占位说明）
    pub too_large: bool,
}

/// 收件游标（settings 的 last_uidvalidity/last_uid 快照）
#[derive(Default, Clone, Copy)]
pub struct LastCursor {
    pub uidvalidity: u32,
    pub uid: u32,
}

/// 同步目标邮箱：收件箱 / 已发送（发件箱先同步，见 fetch_round）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailDest {
    Inbox,
    Sent,
}

/// 一轮收件结果（游标推进依据 = 搜索层 max_uid，解析失败/入库去重不阻塞游标）
#[allow(dead_code)] // 供单测 MockFetcher 的 fetch 实现；生产走 fetch_batched 分批路径
pub struct FetchOutcome {
    /// 服务器 INBOX 的 UIDVALIDITY（空轮也记录，防 uidvalidity 变化后误判）
    pub uidvalidity: u32,
    /// 本轮 UID SEARCH 命中的最大 UID（0 = 空轮，游标保持）
    pub max_uid: u32,
    /// 解析成功的待入库邮件
    pub mails: Vec<IncomingMail>,
}

/// IMAP 通道注入点（生产 imap crate + mailparse；单测注入假实现，与 MailSender 同模式）
pub trait MailFetcher {
    /// 增量拉取：last.uid > 0 且 uidvalidity 未变时从 uid+1 续拉；
    /// uidvalidity 变化或首次按 SINCE 范围（设计 §6.1：提示全量重拉由调用方清理语义）
    #[allow(dead_code)] // 生产 fetch_round 走 RealFetcher::fetch_batched；trait 保留供单测注入
    #[allow(clippy::too_many_arguments)]
    fn fetch(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        first_days: i64,
        last: LastCursor,
        max_mail_bytes: u64,
    ) -> Result<FetchOutcome, String>;

    /// 服务器未读回写（SELECT 校验 uidvalidity 后 STORE +Flags \Seen）
    #[allow(clippy::too_many_arguments)]
    fn mark_seen(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<(), String> {
        self.mark_seen_batch(host, port, mode, account, password, uidvalidity, &[uid])
    }

    /// 批量已读回写：单次连接、单条命令覆盖多个 UID（合并窗口，避免 N 次登录）
    #[allow(clippy::too_many_arguments)]
    fn mark_seen_batch(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<(), String>;
}

/// 生产实现：imap 3.0.0-alpha.15 + mailparse 0.18（MIME 解析，spike 已验证）
pub struct RealFetcher;

/// 首拉范围 UID 搜索（fetch_batched 用；C3 惰性化：仅在无游标首拉、
/// 或增量命中为空需窗口兜底比对时才调用，不占用常规增量轮）。
/// C1：uid_search 返回 HashSet（哈希迭代无序）；调用方必须对结果升序排序，
/// 否则断点游标会越过未入库邮件造成永久漏信。
fn first_pull_search<T: std::io::Read + std::io::Write>(
    sess: &mut imap::Session<T>,
    first_days: i64,
) -> Result<Vec<u32>, String> {
    if first_days <= 0 {
        sess.uid_search("ALL")
            .map_err(|e| format!("检索全部邮件失败：{e}"))
            .map(|s| s.into_iter().collect())
    } else {
        let since = imap_since(first_days);
        sess.uid_search(format!("SINCE {since}"))
            .map_err(|e| format!("检索新邮件失败：{e}"))
            .map(|s| s.into_iter().collect())
    }
}

/// 本轮要拉的 UID 列表（fetch_batched 唯一实现）：
/// 增量优先（C3）——命中 0 封不再回退全窗正文重拉，仅以首拉范围做 UID 级比对
/// 兜底：个别服务器对 N:* 范围有 quirk，若窗口内确有游标之上的新 UID（增量漏报）
/// 则精确补拉这些；全部不大于游标 = 真无新邮件，零正文拉取。
/// C1：结果必须升序（uid_search 返回无序 HashSet），断点游标才只会越过连续已入库前缀。
fn resolve_uids<T: std::io::Read + std::io::Write>(
    sess: &mut imap::Session<T>,
    first_days: i64,
    last: LastCursor,
    uidvalidity: u32,
) -> Result<Vec<u32>, String> {
    let mut uids: Vec<u32> = if last.uid > 0 && last.uidvalidity == uidvalidity {
        let inc: Vec<u32> = sess
            .uid_search(format!("{}:*", last.uid + 1))
            .map_err(|e| format!("检索新邮件失败：{e}"))?
            .into_iter()
            .filter(|u| *u > last.uid)
            .collect();
        if inc.is_empty() {
            first_pull_search(sess, first_days)?
                .into_iter()
                .filter(|u| *u > last.uid)
                .collect()
        } else {
            inc
        }
    } else {
        first_pull_search(sess, first_days)?
    };
    uids.sort_unstable();
    Ok(uids)
}

/// 发现服务器的「已发送」邮箱（发件箱同步目标）。
///
/// 本 imap 版本未导出 LIST 的特殊用途属性（\Sent），按常见命名启发式：
/// 优先全名精确匹配（Sent/Sent Items/Sent Messages/Sent Mail/已发送/已发送邮件/发件箱），
/// 其次名称包含 "sent"（ASCII）或 "已发送"/"发件"。找不到返回 None（跳过该阶段）。
fn find_sent_mailbox<T: std::io::Read + std::io::Write>(
    sess: &mut imap::Session<T>,
) -> Option<String> {
    const EXACT: &[&str] = &[
        "sent",
        "sent items",
        "sent messages",
        "sent mail",
        "已发送",
        "已发送邮件",
        "发件箱",
    ];
    let names = sess.list(None, Some("*")).ok()?;
    let mut best: Option<(u8, String)> = None;
    for n in names.iter() {
        let name = n.name().to_string();
        if name.eq_ignore_ascii_case("INBOX") {
            continue;
        }
        let lower = name.to_lowercase();
        let score = if EXACT.iter().any(|c| lower == *c) {
            0
        } else if lower.contains("sent") || name.contains("已发送") || name.contains("发件") {
            1
        } else {
            continue;
        };
        if best.as_ref().is_none_or(|(s, _)| score < *s) {
            best = Some((score, name));
        }
    }
    best.map(|(_, n)| n)
}

/// IMAP 序列集合（"1,2,3"）：UID STORE/FETCH 的 set 参数
fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// 大小预检：分块 `(UID RFC822.SIZE)`，返回 uid → 字节数。
/// 预检失败不阻断收件（部分服务器对 UID 数据项支持不佳）——退化为"不知道大小"，
/// 按旧行为整封拉取，仍受 BATCH_MAX_BYTES 分批保护。
fn prefetch_sizes<T: std::io::Read + std::io::Write>(
    sess: &mut imap::Session<T>,
    uids: &[u32],
) -> HashMap<u32, u32> {
    let mut out: HashMap<u32, u32> = HashMap::new();
    for chunk in uids.chunks(SIZE_CHECK_CHUNK) {
        match sess.uid_fetch(uid_set(chunk), "(UID RFC822.SIZE)") {
            Ok(fetches) => {
                for f in fetches.iter() {
                    if let (Some(uid), Some(size)) = (f.uid, f.size) {
                        out.insert(uid, size);
                    }
                }
            }
            Err(err) => {
                eprintln!("[fetch] 大小预检失败（按整封拉取继续）：{err}");
                break;
            }
        }
    }
    out
}

impl RealFetcher {
    /// 分批拉取（M6）：单会话内把 uids 按 BATCH 切块，逐批 uid_fetch → 解析 → 回调。
    /// on_batch 回调参数：已解析邮件、累计处理数、总数、uidvalidity、本批最大 UID。
    /// uidvalidity 与 batch_max_uid 用于每批入库后立即持久化游标（需求2 断点续传）。
    #[allow(clippy::too_many_arguments)]
    pub fn fetch_batched(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        dest: MailDest,
        first_days: i64,
        last: LastCursor,
        max_mail_bytes: u64,
        mut on_batch: impl FnMut(&[IncomingMail], usize, usize, u32, u32) -> Result<(), String>,
    ) -> Result<Option<(u32, u32)>, String> {
        let client = imap::ClientBuilder::new(host, port)
            .mode(mode.imap_mode())
            .connect()
            .map_err(|e| format!("连接失败：{e}{}", mode.hint(port)))?;
        let mut sess = client
            .login(account, password)
            .map_err(|(e, _)| format!("登录失败：{e}"))?;
        // 目标邮箱：收件箱固定 INBOX；发件箱按名称启发式发现（特殊用途属性
        // 未在当前 imap 版本导出，按常见命名匹配；找不到则跳过该阶段，不影响收件箱）
        let mailbox_name = match dest {
            MailDest::Inbox => "INBOX".to_string(),
            MailDest::Sent => match find_sent_mailbox(&mut sess) {
                Some(name) => name,
                None => {
                    eprintln!("[fetch] 未发现服务器「已发送」邮箱，跳过发件箱同步");
                    let _ = sess.logout();
                    return Ok(None);
                }
            },
        };
        let mailbox = sess
            .select(&mailbox_name)
            .map_err(|e| format!("无法打开邮箱「{mailbox_name}」：{e}"))?;
        let uidvalidity = mailbox.uid_validity.unwrap_or(0);
        let uids = resolve_uids(&mut sess, first_days, last, uidvalidity)?;
        // 大小预检（审计批 0+2）：超过单封上限的只拉信头，避免整封 + 附件 + gzip
        // 副本同时驻留内存。已知超限 → 占位入库；未知大小 → 按整封拉（旧行为）
        let sizes = prefetch_sizes(&mut sess, &uids);
        // 分 run 拉取：连续同类 UID 归并成一条 FETCH（大邮箱首拉从万次 RTT
        // 降到 千封/千分之一 量级）；升序不变量见 plan_runs 注释
        let runs = plan_runs(&uids, &sizes, max_mail_bytes);
        let total = uids.len();
        let mut processed = 0usize;
        let mut batch: Vec<IncomingMail> = Vec::with_capacity(BATCH);
        let mut batch_uids: Vec<u32> = Vec::with_capacity(BATCH);
        let mut batch_bytes: u64 = 0;
        for run in &runs {
            let query = if run.over {
                "(FLAGS INTERNALDATE BODY.PEEK[HEADER])"
            } else {
                "(FLAGS INTERNALDATE BODY.PEEK[])"
            };
            let fetches = sess.uid_fetch(uid_set(&run.uids), query).map_err(|e| {
                format!(
                    "拉取邮件（{} 封，uid 自 {} 起）失败：{e}",
                    run.uids.len(),
                    run.uids[0]
                )
            })?;
            // 响应顺序由服务器决定：建 uid 索引，仍沿 run 的升序遍历
            let fetched: HashMap<u32, &imap::types::Fetch> = fetches
                .iter()
                .filter_map(|f| f.uid.map(|u| (u, f)))
                .collect();
            for &uid in &run.uids {
                let size = sizes.get(&uid).copied().map(u64::from);
                batch_uids.push(uid);
                processed += 1;
                match fetched.get(&uid) {
                    // 超限：只拿了信头，落占位邮件（服务器对 HEADER 返回空时
                    // placeholder_mail 自带主题兜底，不会显示空行）
                    Some(f) if run.over => {
                        let seen = f
                            .flags()
                            .iter()
                            .any(|flag| flag.to_string().contains("\\Seen"));
                        let internal_date = f.internal_date().map(|dt| dt.timestamp());
                        let header = f.header().unwrap_or_default();
                        batch_bytes += header.len() as u64;
                        batch.push(placeholder_mail(
                            uidvalidity,
                            uid,
                            seen,
                            internal_date,
                            header,
                            size.unwrap_or(0),
                            max_mail_bytes,
                        ));
                    }
                    Some(f) => {
                        let seen = f
                            .flags()
                            .iter()
                            .any(|flag| flag.to_string().contains("\\Seen"));
                        let internal_date = f.internal_date().map(|dt| dt.timestamp());
                        match f.body() {
                            Some(body) => {
                                batch_bytes += body.len() as u64;
                                if let Some(mail) =
                                    parse_incoming(uidvalidity, uid, seen, internal_date, body)
                                {
                                    batch.push(mail);
                                }
                            }
                            None => eprintln!("[fetch] uid={uid} 无 BODY，跳过"),
                        }
                    }
                    // 超限邮件必须有占位行：服务器未返回该 uid 也要落一条，
                    // 否则游标越过它、用户永远看不到这封邮件（静默漏信）
                    None if run.over => {
                        eprintln!("[fetch] uid={uid} 超限但服务器未返回邮件头，落最小占位");
                        batch.push(placeholder_mail(
                            uidvalidity,
                            uid,
                            false,
                            None,
                            &[],
                            size.unwrap_or(0),
                            max_mail_bytes,
                        ));
                    }
                    None => eprintln!("[fetch] uid={uid} 服务器未返回，跳过"),
                }
                // 双阈值 flush：封数或累计字节任一超限即入库，控制解析峰值内存
                if batch_uids.len() >= BATCH || batch_bytes >= BATCH_MAX_BYTES {
                    let bmax = batch_uids.iter().copied().max().unwrap_or(0);
                    on_batch(&batch, processed, total, uidvalidity, bmax)?;
                    batch.clear();
                    batch_uids.clear();
                    batch_bytes = 0;
                }
            }
        }
        if !batch_uids.is_empty() {
            let bmax = batch_uids.iter().copied().max().unwrap_or(0);
            on_batch(&batch, processed, total, uidvalidity, bmax)?;
        }
        let max_uid = uids.iter().copied().max().unwrap_or(0);
        let _ = sess.logout();
        Ok(Some((uidvalidity, max_uid)))
    }

    /// 批量已读回写核心：单次连接 + 单次 SELECT，UID 每 SEEN_CHUNK 一批 STORE。
    /// uidvalidity 不匹配直接报错（UID 空间已重排，旧 UID 指向别的邮件，绝不写入）
    #[allow(clippy::too_many_arguments)]
    pub fn set_seen(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<(), String> {
        if uids.is_empty() {
            return Ok(());
        }
        // 有界探活（同 fetch_round/conncheck）：imap 无连接超时，而 seen worker 是
        // 进程级单线程——连接黑洞（内网丢包不回 RST）会令 worker 永久挂起，
        // 此后所有已读回写停摆直到重启。探活把"不可达"限定在 10s 内交调用方退避
        if let Err(e) = crate::net::probe_tcp(host, port) {
            return Err(format!("{e}；本轮回写未执行"));
        }
        let client = imap::ClientBuilder::new(host, port)
            .mode(mode.imap_mode())
            .connect()
            .map_err(|e| format!("连接失败：{e}{}", mode.hint(port)))?;
        let mut sess = client
            .login(account, password)
            .map_err(|(e, _)| format!("登录失败：{e}"))?;
        let mailbox = sess
            .select("INBOX")
            .map_err(|e| format!("无法打开收件箱：{e}"))?;
        if mailbox.uid_validity.unwrap_or(0) != uidvalidity {
            let _ = sess.logout();
            return Err(format!(
                "{ERR_UIDVALIDITY_CHANGED}，跳过回写（下次收件自动对齐）"
            ));
        }
        for chunk in uids.chunks(SEEN_CHUNK) {
            sess.uid_store(uid_set(chunk), "+FLAGS (\\Seen)")
                .map_err(|e| format!("回写已读失败：{e}"))?;
        }
        let _ = sess.logout();
        Ok(())
    }
}

impl MailFetcher for RealFetcher {
    /// 整包拉取（供单测/同步调用）：内部复用 `fetch_batched`，不再另写一套
    /// 搜索/拉取/解析（两处重复已导致修复漂移，见模块头注释）
    #[allow(clippy::too_many_arguments)]
    fn fetch(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        first_days: i64,
        last: LastCursor,
        max_mail_bytes: u64,
    ) -> Result<FetchOutcome, String> {
        let mut mails: Vec<IncomingMail> = Vec::new();
        let mut last_uv = 0u32;
        let mut last_max = 0u32;
        let (uidvalidity, max_uid) = self
            .fetch_batched(
                host,
                port,
                mode,
                account,
                password,
                MailDest::Inbox,
                first_days,
                last,
                max_mail_bytes,
                |batch, _done, _total, uv, batch_max| {
                    mails.extend_from_slice(batch);
                    last_uv = uv;
                    last_max = last_max.max(batch_max);
                    Ok(())
                },
            )?
            .ok_or_else(|| "收件箱不可用".to_string())?;
        Ok(FetchOutcome {
            uidvalidity: if last_uv == 0 { uidvalidity } else { last_uv },
            max_uid: max_uid.max(last_max),
            mails,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn mark_seen_batch(
        &self,
        host: &str,
        port: u16,
        mode: TlsMode,
        account: &str,
        password: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<(), String> {
        self.set_seen(host, port, mode, account, password, uidvalidity, uids)
    }
}

/// IMAP SINCE 日期（"DD-Mon-YYYY"，epoch 换算）
fn imap_since(days: i64) -> String {
    let cutoff = now_epoch() - days.clamp(1, 365) * 86_400;
    let Some(z) = zoned_of(cutoff) else {
        return "01-Jan-2020".into();
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(z.month() as usize - 1).min(11)];
    format!("{:02}-{month}-{}", z.day() as u32, z.year())
}

/// 头部字段抽取（parse_incoming 与 placeholder_mail 共用）：
/// 主题/地址/Message-Id（RFC2047 解码由 mailparse 完成）
struct HeaderFields {
    subject: String,
    from_addr: String,
    from_name: String,
    to_addrs: String,
    cc_addrs: String,
    message_id: String,
}

fn header_fields(part: &mailparse::ParsedMail) -> HeaderFields {
    use mailparse::MailHeaderMap;
    let headers = part.get_headers();
    // RFC2047 解码需 get_first_header + get_value（get_first_value 返回原始值不解码）
    let header_value = |key: &str| -> String {
        headers
            .get_first_header(key)
            .map(|h| h.get_value())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    // From/To：addrparse 解析出地址与显示名
    let (from_addr, from_name) = headers
        .get_first_header("From")
        .and_then(|h| mailparse::addrparse_header(h).ok())
        .and_then(|list| {
            list.extract_single_info().map(|i| {
                (
                    i.addr.to_string(),
                    i.display_name.clone().unwrap_or_default(),
                )
            })
        })
        .unwrap_or_default();
    // To/Cc：addrparse 展开组地址，取纯地址逗号拼接（联系人采集与展示共用）
    let addr_list = |key: &str| -> String {
        headers
            .get_first_header(key)
            .and_then(|h| mailparse::addrparse_header(h).ok())
            .map(|list| {
                let mut out: Vec<String> = Vec::new();
                for a in list.iter() {
                    match a {
                        mailparse::MailAddr::Single(s) => out.push(s.addr.clone()),
                        mailparse::MailAddr::Group(g) => {
                            for s in &g.addrs {
                                out.push(s.addr.clone());
                            }
                        }
                    }
                }
                out.join(",")
            })
            .unwrap_or_default()
    };
    let to_addrs = addr_list("To");
    let cc_addrs = addr_list("Cc");
    HeaderFields {
        subject: header_value("Subject"),
        from_addr,
        from_name,
        to_addrs,
        cc_addrs,
        message_id: header_value("Message-Id"),
    }
}

/// 超大邮件的占位形态：只有信头派生字段 + 一段说明正文，附件与正文均不下载。
/// 明确告诉用户"邮件在服务器上、本地只存了占位"，而不是显示一封空邮件
fn placeholder_mail(
    uidvalidity: u32,
    uid: u32,
    seen: bool,
    internal_date: Option<i64>,
    header: &[u8],
    size: u64,
    max_mail_bytes: u64,
) -> IncomingMail {
    // 注意 parse_mail(空输入) 返回 Ok(空头)（mailparse parse_headers 对空数据直接
    // break），因此"服务器未返回信头"走不到下面的 unwrap_or 分支——空主题在此兜底
    let mut fields = mailparse::parse_mail(header)
        .map(|m| header_fields(&m))
        .unwrap_or(HeaderFields {
            subject: "（邮件头无法解析）".into(),
            from_addr: String::new(),
            from_name: String::new(),
            to_addrs: String::new(),
            cc_addrs: String::new(),
            message_id: String::new(),
        });
    if fields.subject.is_empty() {
        fields.subject = "（未获取到邮件主题）".into();
    }
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let body_text = if size > 0 {
        format!(
            "（该邮件约 {:.1} MB，超过本地单封收件上限 {:.0} MB，正文与附件未下载。\
             可用网页邮箱查看完整内容，或调高「设置 → 收件同步 → 单封收件上限」后重新拉取。）",
            mb(size),
            mb(max_mail_bytes)
        )
    } else {
        format!(
            "（该邮件超过本地单封收件上限 {:.0} MB，正文与附件未下载。\
             可用网页邮箱查看完整内容，或调高「设置 → 收件同步 → 单封收件上限」后重新拉取。）",
            mb(max_mail_bytes)
        )
    };
    IncomingMail {
        uidvalidity,
        uid,
        seen,
        internal_date,
        message_id: fields.message_id,
        subject: fields.subject,
        from_addr: fields.from_addr,
        from_name: fields.from_name,
        to_addrs: fields.to_addrs,
        cc_addrs: fields.cc_addrs,
        body_text,
        body_kind: "plain".into(),
        attachments: Vec::new(),
        too_large: true,
    }
}

/// MIME 解析为 IncomingMail（mailparse：主题/地址自动解码 RFC2047，spike 已验证）
fn parse_incoming(
    uidvalidity: u32,
    uid: u32,
    seen: bool,
    internal_date: Option<i64>,
    raw: &[u8],
) -> Option<IncomingMail> {
    let mail = mailparse::parse_mail(raw).ok()?;
    let fields = header_fields(&mail);
    let (body_text, body_kind) = extract_body(&mail);
    let attachments = collect_attachments(&mail);
    Some(IncomingMail {
        uidvalidity,
        uid,
        seen,
        internal_date,
        message_id: fields.message_id,
        subject: fields.subject,
        from_addr: fields.from_addr,
        from_name: fields.from_name,
        to_addrs: fields.to_addrs,
        cc_addrs: fields.cc_addrs,
        body_text,
        body_kind: body_kind.to_string(),
        attachments,
        too_large: false,
    })
}

/// cp1252 高位区（0x80-0x9F 的 Unicode 映射）反查表：mojibake 修复用
fn cp1252_high_inverse(c: char) -> Option<u8> {
    Some(match c {
        '\u{20AC}' => 0x80,
        '\u{201A}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201E}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02C6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8A,
        '\u{2039}' => 0x8B,
        '\u{0152}' => 0x8C,
        '\u{017D}' => 0x8E,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201C}' => 0x93,
        '\u{201D}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02DC}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9A,
        '\u{203A}' => 0x9B,
        '\u{0153}' => 0x9C,
        '\u{017E}' => 0x9E,
        '\u{0178}' => 0x9F,
        _ => return None,
    })
}

/// 正文 mojibake 修复（M5 验收）：部分邮件网关把 UTF-8 正文误标为
/// iso-8859-1/windows-1252，mailparse 按声明逐字节解码产生乱码（如"原文"→"åŽŸæ–‡"）。
/// 探测：文本全部字符可逆映射回 cp1252 字节、且逆推字节恰为合法 UTF-8、含 CJK——
/// 判定原文实为 UTF-8，返回重解码结果。正常西文邮件逆推后 UTF-8 解码几乎必失败；
/// 含正常中文的文本提前退出，双重防误伤。
fn fix_mojibake(body: &str) -> Option<String> {
    let mut has_high = false;
    for c in body.chars() {
        let u = c as u32;
        if u >= 0x80 {
            has_high = true;
            if is_cjk(c) {
                return None; // 已含正常中文：解码正确（mojibake 不可能恰含 CJK）
            }
        }
    }
    if !has_high {
        return None; // 纯 ASCII 无需修复
    }
    let mut bytes = Vec::with_capacity(body.len());
    for c in body.chars() {
        let u = c as u32;
        // Latin-1 直通（iso-8859-1 解码路径：0x80-0x9F 为 C1 控制字符直通）
        // + cp1252 特有映射（windows-1252 解码路径：如 U+017D→0x8E）——两条路径都可逆
        let b = if u <= 0xFF {
            u as u8
        } else {
            cp1252_high_inverse(c)?
        };
        bytes.push(b);
    }
    let fixed = String::from_utf8(bytes).ok()?;
    (fixed != body && fixed.chars().any(is_cjk)).then_some(fixed)
}

/// 未声明/错误声明 charset 的 GBK 正文兜底：按声明解码丢字（U+FFFD）或落成无 CJK
/// 高位乱码时，取原始字节（已解传输编码）按 GBK 重解码；无错且含中文才采纳
/// （正常西文邮件 GBK 解码必然报错，不会误伤）
fn fix_gbk_fallback(part: &mailparse::ParsedMail, decoded: &str) -> String {
    let suspect = decoded.contains('\u{FFFD}')
        || (!decoded.chars().any(is_cjk) && decoded.chars().any(|c| (c as u32) >= 0x80));
    if !suspect {
        return decoded.to_string();
    }
    let Ok(raw) = part.get_body_raw() else {
        return decoded.to_string();
    };
    // 原始字节本就是合法 UTF-8：丢字另有原因（如 us-ascii 声明截断），不猜测
    if std::str::from_utf8(&raw).is_ok() {
        return decoded.to_string();
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(&raw);
    if !had_errors && cow.chars().any(is_cjk) {
        cow.into_owned()
    } else {
        decoded.to_string()
    }
}

/// 取正文文本并修复编码问题：mojibake（UTF-8 被误标 latin1/1252）优先，GBK 兜底其次
fn body_text_repaired(part: &mailparse::ParsedMail) -> String {
    let decoded = part.get_body().unwrap_or_default();
    if let Some(fixed) = fix_mojibake(&decoded) {
        return fixed;
    }
    fix_gbk_fallback(part, &decoded)
}

/// 正文抽取（§6.1）：multipart 递归，text/plain 优先；仅 text/html 时剥标签存纯文本；
/// 编码修复（M5）：mis-declared charset 修复 + GBK 兜底
fn extract_body(mail: &mailparse::ParsedMail) -> (String, &'static str) {
    if let Some(p) = find_inline_part(mail, "text/plain") {
        return (body_text_repaired(p), "plain");
    }
    if let Some(p) = find_inline_part(mail, "text/html") {
        return (strip_html(&body_text_repaired(p)), "html");
    }
    (String::new(), "plain")
}

fn find_inline_part<'a>(
    mail: &'a mailparse::ParsedMail<'a>,
    mime: &str,
) -> Option<&'a mailparse::ParsedMail<'a>> {
    if mail.ctype.mimetype == mime
        && mail.get_content_disposition().disposition != mailparse::DispositionType::Attachment
    {
        return Some(mail);
    }
    for sub in &mail.subparts {
        if let Some(p) = find_inline_part(sub, mime) {
            return Some(p);
        }
    }
    None
}

/// HTML 剥标签（无 regex 依赖的手工实现）+ 基础实体解码。
/// 索引扫描而非吞并式迭代：未闭合 `<` 不应吞掉后文（畸形 HTML 常见）
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        match html.as_bytes()[i] {
            b'<' => match html[i..].find('>') {
                Some(rel) => i += rel + 1, // 整段标签丢弃
                None => {
                    // 未闭合：保留 '<' 与后文（不解析）
                    out.push_str(&html[i..]);
                    break;
                }
            },
            b'&' => {
                // 实体：扫描到 ';'（最多 8 字符）；未闭合则原样输出
                let rest = &html[i + 1..];
                match rest.find(';').filter(|pos| *pos <= 8) {
                    Some(pos) => {
                        match &rest[..pos] {
                            "nbsp" => out.push(' '),
                            "lt" => out.push('<'),
                            "gt" => out.push('>'),
                            "amp" => out.push('&'),
                            "quot" => out.push('"'),
                            other => out.push_str(other),
                        }
                        i += 1 + pos + 1;
                    }
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            _ => {
                let ch = html[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// 附件收集（§6.1：Content-Disposition attachment 或带 filename 的 part）
fn collect_attachments(mail: &mailparse::ParsedMail) -> Vec<IncomingAttach> {
    let mut out = Vec::new();
    walk_attachments(mail, &mut out);
    out
}

fn walk_attachments(mail: &mailparse::ParsedMail, out: &mut Vec<IncomingAttach>) {
    let cd = mail.get_content_disposition();
    let named = cd.params.contains_key("filename") || mail.ctype.params.contains_key("name");
    if cd.disposition == mailparse::DispositionType::Attachment || named {
        let filename = cd
            .params
            .get("filename")
            .or_else(|| mail.ctype.params.get("name"))
            .cloned()
            .unwrap_or_else(|| "attachment".into());
        if let Ok(data) = mail.get_body_raw() {
            out.push(IncomingAttach {
                filename,
                mime: mail.ctype.mimetype.clone(),
                data,
            });
        }
        return; // 附件 part 不再递归
    }
    for sub in &mail.subparts {
        walk_attachments(sub, out);
    }
}

/// 收件入库（M4 §6.1）：remote_uid（UNIQUE）+ message_id 双重去重；分词影子列；
/// unread 按 \Seen；附件走两级存储。返回新增封数。
/// （生产走收件线程的 conn 版；本 AppState 薄壳服务于单测与同步调用）
#[cfg_attr(not(test), allow(dead_code))]
pub fn store_incoming(state: &AppState, mails: &[IncomingMail]) -> Result<usize, String> {
    let data_dir = state.data_dir();
    state.with_conn(|conn| store_incoming_conn(conn, &data_dir, mails, MailDest::Inbox))
}

/// 收件入库核心（独立连接版：收件线程使用，不依赖 AppState）。
/// 每封邮件一个事务：正文行 + 附件行原子提交（避免"正文已入库、附件缺失"的半状态）；
/// 单封失败回滚该封并中止整批（已提交的前序邮件保留，与 §6.1 取舍一致）。
pub fn store_incoming_conn(
    conn: &mut rusqlite::Connection,
    data_dir: &Path,
    mails: &[IncomingMail],
    dest: MailDest,
) -> Result<usize, String> {
    let root = attach_root_of(data_dir);
    let limit = inline_limit_conn(conn);
    let mut added = 0usize;
    // 分目标形态：收件箱（in/inbox/received/未读跟随 \Seen）；
    // 发件箱（out/sent/sent，恒已读）。remote_uid 加 "sent:" 前缀——
    // UIDVALIDITY 按邮箱独立，两箱的 v/u 可能相撞，前缀防唯一索引冲突。
    let (direction, folder, status, unread_of, remote_prefix) = match dest {
        MailDest::Inbox => ("in", "inbox", "received", None, ""),
        MailDest::Sent => ("out", "sent", "sent", Some(false), "sent:"),
    };
    for m in mails {
        let remote = format!("{remote_prefix}{}:{}", m.uidvalidity, m.uid);
        // 去重：remote_uid 唯一索引 + message_id 双保险（空串不按 message_id 去重）。
        // message_id 限定同方向：自己发给自己的信会同时出现在 Sent 与 Inbox，
        // 跨方向去重会吞掉其中一份。
        let dup: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM emails WHERE remote_uid = ?1)
                     OR (?2 <> '' AND EXISTS(
                           SELECT 1 FROM emails WHERE message_id = ?2 AND direction = ?3))",
                rusqlite::params![remote, m.message_id, direction],
                |r| r.get(0),
            )
            .map_err(e)?;
        if dup {
            continue;
        }
        let st = tokenize(&m.subject);
        let bt = tokenize(&m.body_text);
        let now = now_epoch();
        let attach_bytes: i64 = m.attachments.iter().map(|a| a.data.len() as i64).sum();
        let unread = unread_of.unwrap_or(!m.seen);
        let ts = m.internal_date.unwrap_or(now);
        let (received_at, sent_at) = match dest {
            MailDest::Inbox => (Some(ts), None),
            MailDest::Sent => (None, Some(ts)),
        };

        let tx = conn.transaction().map_err(e)?;
        tx.execute(
            "INSERT INTO emails(direction, folder, status, from_addr, from_name, to_addrs,
                                cc_addrs, subject, body_text, body_kind, subject_tokens,
                                body_tokens, attach_cnt, attach_bytes, unread, remote_uid,
                                message_id, created_at, received_at, sent_at, too_large)
             VALUES (?17, ?18, ?19, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?20, ?21, ?22)",
            rusqlite::params![
                m.from_addr,
                m.from_name,
                m.to_addrs,
                m.cc_addrs,
                m.subject,
                m.body_text,
                m.body_kind,
                st,
                bt,
                m.attachments.len() as i64,
                attach_bytes,
                unread,
                remote,
                m.message_id,
                now,
                ts,
                direction,
                folder,
                status,
                received_at,
                sent_at,
                m.too_large,
            ],
        )
        .map_err(e)?;
        let id = tx.last_insert_rowid();
        let month = month_dir_conn(&tx, id);
        let mut written: Vec<PathBuf> = Vec::new();
        let stored = (|| -> Result<(), String> {
            for a in &m.attachments {
                insert_attachment(
                    &AttachCtx {
                        tx: &tx,
                        root: &root,
                        month: &month,
                        email_id: id,
                        limit,
                    },
                    &safe_filename(&a.filename),
                    &a.mime,
                    &a.data,
                    &mut written,
                )?;
            }
            sync_attach_stats(&tx, id)
        })();
        let stored = stored.and_then(|()| {
            // 联系人采集（v4）：From（带姓名）+ To + Cc，与邮件行同事务落库
            super::contacts::upsert_conn(&tx, &m.from_addr, &m.from_name, now).map_err(e)?;
            for (addr, name) in super::contacts::parse_addr_list(&m.to_addrs) {
                super::contacts::upsert_conn(&tx, &addr, &name, now).map_err(e)?;
            }
            for (addr, name) in super::contacts::parse_addr_list(&m.cc_addrs) {
                super::contacts::upsert_conn(&tx, &addr, &name, now).map_err(e)?;
            }
            Ok(())
        });
        match stored {
            Ok(()) => {
                tx.commit().map_err(e)?;
                added += 1;
            }
            Err(err) => {
                // 事务 drop 即回滚；清理本封已写盘的独立附件文件
                drop(tx);
                for f in &written {
                    let _ = std::fs::remove_file(f);
                }
                return Err(err);
            }
        }
    }
    Ok(added)
}

/// 本地已读 + 服务器 \Seen 回写（§6.1）：
/// 参数快照在调用线程（协议回调内，纯本地查询）完成后，
/// 网络回写交由调用方 spawn 的线程执行（fetcher 不依赖 AppState）。
/// 同步语义版（单测验证回写参数）；生产路径见 seen worker（mail::seen）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn mark_seen(state: &AppState, fetcher: &dyn MailFetcher, id: i64) -> Result<(), String> {
    // 本地置已读（幂等；无 remote_uid 时仅本地）
    mark_read(state, id);
    let Some(p) = seen_params_of(state, id) else {
        return Ok(()); // 本地邮件（草稿/已发送）或凭据缺失：仅本地已读
    };
    fetcher.mark_seen(
        &p.host,
        p.port,
        p.security,
        &p.account,
        &p.password,
        p.uidvalidity,
        p.uid,
    )
}

/// 已读回写的参数快照（无网络操作；worker 取快照后可自由移交线程）
pub struct SeenParams {
    pub host: String,
    pub port: u16,
    pub security: TlsMode,
    pub account: String,
    pub password: String,
    pub uidvalidity: u32,
    pub uid: u32,
}

/// 取回写参数（本地已读由 mark_read 负责；此处读 remote/服务器参数/凭据）。
/// 无 remote_uid（本地草稿/out 已发送）或凭据缺失返回 None（静默跳过回写）。
pub fn seen_params_of(state: &AppState, id: i64) -> Option<SeenParams> {
    // 单层 ?：闭包返回 Option<String>（None = 非 in 邮件/已删除），None 即跳过回写
    let remote = state.with_conn(|conn| {
        conn.query_row(
            "SELECT remote_uid FROM emails WHERE id = ?1 AND direction = 'in'",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None)
    })?;
    let (v, u) = remote.split_once(':')?;
    let uidvalidity = v.parse().ok()?;
    let uid = u.parse().ok()?;
    let d = defaults();
    let host = state.get_or(keys::RECV_HOST, &d).unwrap_or_default();
    let port: u16 = state
        .get_or(keys::RECV_PORT, &d)
        .and_then(|p| p.parse().ok())
        .unwrap_or(993);
    let security = TlsMode::resolve(
        port,
        &state.get_or(keys::RECV_SECURITY, &d).unwrap_or_default(),
    );
    let prefix = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let account = full_address(&prefix, &domain);
    let password = match state.retrieve_password(&account) {
        crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
        crate::secrets::GetResult::Missing => return None,
    };
    Some(SeenParams {
        host,
        port,
        security,
        account,
        password,
        uidvalidity,
        uid,
    })
}

#[cfg(test)]
#[path = "../tests/mail_recv.rs"]
mod tests;
