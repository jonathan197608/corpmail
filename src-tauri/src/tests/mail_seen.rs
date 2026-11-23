//! 已读回写批处理单测（src/mail/seen.rs 内联测试，经 #[path] 引入）

use super::*;
use crate::mail::{
    IncomingMail, LastCursor, ensure_seen_recheck, mark_read, mark_unread, store_incoming,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 假收件器：记录批量回写调用（host/port/账号/uidvalidity/uids），可注入错误
type BatchCall = (String, u16, String, u32, Vec<u32>);

struct FakeFetcher {
    calls: Mutex<Vec<BatchCall>>,
    /// 回写返回的错误（None = 成功）
    fail: Option<String>,
}

impl FakeFetcher {
    fn ok() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: None,
        }
    }

    fn failing(msg: &str) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: Some(msg.to_string()),
        }
    }

    fn calls(&self) -> Vec<BatchCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl MailFetcher for FakeFetcher {
    fn fetch(
        &self,
        _host: &str,
        _port: u16,
        _mode: crate::net::TlsMode,
        _account: &str,
        _password: &str,
        _first_days: i64,
        _last: LastCursor,
        _max_mail_bytes: u64,
    ) -> Result<crate::mail::FetchOutcome, String> {
        Ok(crate::mail::FetchOutcome {
            uidvalidity: 0,
            max_uid: 0,
            mails: Vec::new(),
        })
    }

    fn mark_seen_batch(
        &self,
        host: &str,
        port: u16,
        _mode: crate::net::TlsMode,
        account: &str,
        _password: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<(), String> {
        self.calls.lock().unwrap().push((
            host.into(),
            port,
            account.into(),
            uidvalidity,
            uids.to_vec(),
        ));
        match &self.fail {
            Some(msg) => Err(msg.clone()),
            None => Ok(()),
        }
    }
}

fn temp_state(tag: &str) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("corpmail-seen-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (AppState::open(&dir), dir)
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn incoming(uidvalidity: u32, uid: u32) -> IncomingMail {
    IncomingMail {
        uidvalidity,
        uid,
        seen: false,
        internal_date: Some(1_700_000_000),
        message_id: format!("<m-{uidvalidity}-{uid}@x.cn>"),
        subject: format!("来件 {uid}"),
        from_addr: "boss@x.cn".into(),
        from_name: "领导".into(),
        cc_addrs: String::new(),
        to_addrs: "demo@chinatelecom.cn".into(),
        body_text: "正文".into(),
        body_kind: "plain".into(),
        body_html: None,
        raw_headers: String::new(),
        attachments: Vec::new(),
        too_large: false,
        placeholder: false,
    }
}

/// 建好服务器参数 + 凭据 + 若干未读来件，返回 (state, dir, 邮件 id 列表)
fn fixture(tag: &str, uids: &[(u32, u32)]) -> (AppState, PathBuf, Vec<i64>) {
    let (state, dir) = temp_state(tag);
    state
        .set_batch(&[
            (crate::state::keys::RECV_HOST, "imap.test.cn"),
            (crate::state::keys::RECV_PORT, "993"),
            (crate::state::keys::ACCOUNT_PREFIX, "demo"),
            (crate::state::keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
        ])
        .unwrap();
    state
        .store_password("demo@chinatelecom.cn", "pw-seen")
        .unwrap();
    let mails: Vec<IncomingMail> = uids.iter().map(|(uv, u)| incoming(*uv, *u)).collect();
    store_incoming(&state, &mails).unwrap();
    let ids: Vec<i64> = state.with_conn(|c| {
        c.prepare("SELECT id FROM emails ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    });
    (state, dir, ids)
}

fn pending_count(state: &AppState) -> i64 {
    state.with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(SUM(seen_pending), 0) FROM emails",
            [],
            |r| r.get(0),
        )
        .unwrap()
    })
}

/// 连点 N 封 → 一次 flush 只发一次批量 STORE，UID 齐全且标志清空
#[test]
fn flush_batches_all_pending_into_single_call() {
    let (state, dir, ids) = fixture("batch", &[(42, 1), (42, 2), (42, 3)]);
    for id in &ids {
        assert!(mark_read(&state, *id).unwrap(), "首次置已读应生效");
    }
    assert_eq!(pending_count(&state), 3, "本地已读应同时置待回写标志");

    let fetcher = FakeFetcher::ok();
    let done = flush(&state, &fetcher).unwrap();
    assert_eq!(done, 3);
    let calls = fetcher.calls();
    assert_eq!(calls.len(), 1, "三封合并为一次连接/一次 STORE");
    assert_eq!(calls[0].0, "imap.test.cn");
    assert_eq!(calls[0].1, 993);
    assert_eq!(calls[0].2, "demo@chinatelecom.cn");
    assert_eq!(calls[0].3, 42);
    assert_eq!(calls[0].4, vec![1, 2, 3]);
    assert_eq!(pending_count(&state), 0, "成功后标志应清空");

    // 已读状态不受回写影响（本地 unread=0 保持）
    let unread: i64 = state.with_conn(|c| {
        c.query_row("SELECT SUM(unread) FROM emails", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(unread, 0);
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 复查四 P2-2：出队后用户又标未读的行不得进入 STORE +\Seen
#[test]
fn flush_skips_rows_marked_unread_after_enqueue() {
    let (state, dir, ids) = fixture("race-unread", &[(42, 1), (42, 2), (42, 3)]);
    for id in &ids {
        mark_read(&state, *id).unwrap();
    }
    // 模拟用户在合并窗口内点了「标为未读」，但尚未清理 pending 标志的边界态
    state
        .with_conn(|c| {
            c.execute("UPDATE emails SET unread = 1 WHERE id = ?1", [ids[1]])
        })
        .unwrap();
    let fetcher = FakeFetcher::ok();
    let done = flush(&state, &fetcher).unwrap();
    assert_eq!(done, 2, "变未读的行不得回写");
    let calls = fetcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].4, vec![1, 3], "不得对已标未读的 UID 发 STORE +Seen");
    let (unread, pending): (i64, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT unread, seen_pending FROM emails WHERE id = ?1",
            [ids[1]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!((unread, pending), (1, 0), "跳过的行保留未读且不留悬挂标志");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 网络失败：标志保留（供退避重试），错误向上传递
#[test]
fn flush_keeps_flag_on_transient_failure() {
    let (state, dir, ids) = fixture("fail", &[(42, 7)]);
    mark_read(&state, ids[0]).unwrap();
    let fetcher = FakeFetcher::failing("回写已读失败：连接中断");
    let err = flush(&state, &fetcher).unwrap_err();
    assert!(err.contains("连接中断"), "{err}");
    assert_eq!(pending_count(&state), 1, "可重试失败必须保留标志");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// uidvalidity 变化：清标志放弃（旧 UID 可能指向别的邮件，绝不写入）
#[test]
fn flush_clears_flag_on_uidvalidity_change() {
    let (state, dir, ids) = fixture("uv", &[(42, 9)]);
    mark_read(&state, ids[0]).unwrap();
    let fetcher = FakeFetcher::failing(&format!(
        "{}，跳过回写（下次收件自动对齐）",
        ERR_UIDVALIDITY_CHANGED
    ));
    let done = flush(&state, &fetcher).unwrap();
    assert_eq!(done, 0, "未成功回写任何一封");
    assert_eq!(pending_count(&state), 0, "UID 重排后不得继续重试");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 不同 uidvalidity 分组回写（UID 空间不同，不能混在一条命令里）
#[test]
fn flush_groups_by_uidvalidity() {
    let (state, dir, ids) = fixture("group", &[(42, 1), (43, 1)]);
    for id in &ids {
        mark_read(&state, *id).unwrap();
    }
    let fetcher = FakeFetcher::ok();
    assert_eq!(flush(&state, &fetcher).unwrap(), 2);
    let calls = fetcher.calls();
    assert_eq!(calls.len(), 2, "两个 uidvalidity 各自一次");
    let uvs: Vec<u32> = calls.iter().map(|c| c.3).collect();
    assert_eq!(uvs, vec![42, 43]);
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 无待回写行：不触网（空轮直接返回 0）
#[test]
fn flush_noop_when_nothing_pending() {
    let (state, dir, _) = fixture("noop", &[(42, 1)]);
    let fetcher = FakeFetcher::ok();
    assert_eq!(flush(&state, &fetcher).unwrap(), 0);
    assert!(fetcher.calls().is_empty());
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 放弃语义：清掉全部标志（本地已读保持）
#[test]
fn abandon_clears_all_pending() {
    let (state, dir, ids) = fixture("abandon", &[(42, 1), (42, 2)]);
    for id in &ids {
        mark_read(&state, *id).unwrap();
    }
    assert_eq!(pending_count(&state), 2);
    abandon(&state).unwrap();
    assert_eq!(pending_count(&state), 0);
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// 退避：第 1 次失败 60s 起，翻倍到上限 10 分钟
#[test]
fn backoff_grows_and_caps() {
    assert_eq!(backoff(1).as_secs(), 60, "首次失败 60s 起");
    assert_eq!(backoff(2).as_secs(), 120);
    assert_eq!(backoff(3).as_secs(), 240);
    assert_eq!(backoff(4).as_secs(), 480);
    assert_eq!(backoff(5).as_secs(), 600, "上限 10 分钟");
    assert_eq!(backoff(99).as_secs(), 600);
}

/// 本地草稿/已发送（无 remote_uid）：不置标志，不触发回写
#[test]
fn local_mail_never_pends() {
    let (state, dir) = temp_state("local");
    let id = crate::mail::save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "草稿".into(),
            body_text: "体".into(),
            in_reply_to: None,
            scheduled_at: 0,
        },
    )
    .unwrap();
    mark_read(&state, id).unwrap(); // 草稿 unread=0：本来就不翻转
    assert_eq!(pending_count(&state), 0);
    cleanup(&dir);
}

/// 畸形 remote_uid 行（手改库形态）：清标志跳过——不占 LIMIT 窗口、
/// 不阻塞正常行回写、不触发自唤醒空转
#[test]
fn malformed_remote_uid_rows_are_cleared_not_stuck() {
    let (state, dir) = temp_state("malformed-uid");
    state
        .set_batch(&[
            (crate::state::keys::RECV_HOST, "imap.test.cn"),
            (crate::state::keys::RECV_PORT, "993"),
            (crate::state::keys::ACCOUNT_PREFIX, "demo"),
            (crate::state::keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
        ])
        .unwrap();
    state
        .store_password("demo@chinatelecom.cn", "pw-seen")
        .unwrap();
    // 畸形行排前（小 id，ORDER BY id LIMIT 窗口头部），正常待回写行排后
    state
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO emails(id, direction, folder, status, from_addr, to_addrs,
                                    created_at, remote_uid, seen_pending, unread)
                 VALUES (1, 'in', 'inbox', 'received', 'a@x.cn', 'b@x.cn',
                         1, 'garbage', 1, 0)",
                [],
            )
        })
        .unwrap();
    let mails = vec![incoming(42, 7)];
    store_incoming(&state, &mails).unwrap();
    let valid_id: i64 = state.with_conn(|conn| {
        conn.query_row("SELECT id FROM emails WHERE remote_uid = '42:7'", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert!(mark_read(&state, valid_id).unwrap(), "正常行应置为待回写");

    let fetcher = FakeFetcher::ok();
    assert_eq!(flush(&state, &fetcher).unwrap(), 1, "只有正常行计入回写");
    assert_eq!(fetcher.calls().len(), 1, "畸形行不得产生网络调用");
    assert_eq!(fetcher.calls()[0].4, vec![7]);
    let stuck: i64 = state.with_conn(|conn| {
        conn.query_row(
            "SELECT count(*) FROM emails WHERE seen_pending = 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(stuck, 0, "畸形行与正常行的标志都应清零");
    cleanup(&dir);
}

/// R13：换账号后旧账号的 pending 行不参与回写——旧 UID 在新账号邮箱里
/// 没有意义（STORE 会误标别人的邮件）；标志保留，切回原账号后照常回写
#[test]
fn flush_skips_other_accounts_pending_rows() {
    let (state, dir, ids) = fixture("acct", &[(42, 1)]);
    mark_read(&state, ids[0]).unwrap();
    assert_eq!(pending_count(&state), 1);

    // 换账号（只改配置）：旧账号的行不得被选中
    state
        .set_batch(&[(crate::state::keys::ACCOUNT_PREFIX, "other")])
        .unwrap();
    let fetcher = FakeFetcher::ok();
    assert_eq!(flush(&state, &fetcher).unwrap(), 0, "旧账号的行不得回写");
    assert!(fetcher.calls().is_empty(), "不得发起任何 IMAP 调用");
    assert_eq!(pending_count(&state), 1, "标志保留：等切回原账号再回写");

    // 切回原账号：回写恢复
    state
        .set_batch(&[(crate::state::keys::ACCOUNT_PREFIX, "demo")])
        .unwrap();
    assert_eq!(flush(&state, &fetcher).unwrap(), 1);
    assert_eq!(pending_count(&state), 0);
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// R13：置标志时记录归属账号（mark_read 同一 UPDATE 写入）
#[test]
fn mark_read_records_account_on_pending_rows() {
    let (state, dir, ids) = fixture("acct-rec", &[(42, 5)]);
    mark_read(&state, ids[0]).unwrap();
    let account: Option<String> = state.with_conn(|c| {
        c.query_row(
            "SELECT seen_account FROM emails WHERE id = ?1",
            [ids[0]],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(account.as_deref(), Some("demo@chinatelecom.cn"));
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// C5：批量「标记已读」必须与单封 mark_read 同口径写 seen_pending + 归属账号，
/// 否则本地已读、服务器仍 \Seen 未置，换设备/webmail 看到仍未读
#[test]
fn batch_mark_read_queues_seen_sync_for_current_account() {
    let (state, dir, ids) = fixture("batch-read", &[(42, 1), (42, 2)]);
    let n = crate::mail::batch_apply(
        &state,
        &crate::mail::BatchTarget::Ids(ids.clone()),
        crate::mail::BatchAction::MarkRead,
    )
    .unwrap();
    assert_eq!(n, 2);
    assert_eq!(pending_count(&state), 2, "批量也要写 seen_pending");
    let accounts: Vec<String> = state.with_conn(|c| {
        let mut stmt = c
            .prepare("SELECT COALESCE(seen_account, '') FROM emails ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    });
    assert!(
        accounts.iter().all(|a| a == "demo@chinatelecom.cn"),
        "归属账号必须随批量写入，供 R13 过滤"
    );
    assert_eq!(
        pending_rows(&state).unwrap().len(),
        2,
        "worker 必须能选出这些行"
    );
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// C6：abandon 是「放弃」出口，必须与 pending_rows 一样按账号过滤。
/// 当前账号 B 回写失败时清掉 A 的悬挂标志 = A 的服务器已读同步永久丢失。
#[test]
fn abandon_keeps_other_accounts_pending_rows() {
    let (state, dir, ids) = fixture("abandon-acct", &[(42, 1), (42, 2)]);
    mark_read(&state, ids[0]).unwrap();
    mark_read(&state, ids[1]).unwrap();
    // ids[0] 模拟账号 A 遗留的悬挂意向；当前账号仍是 fixture 的 demo（B）
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET seen_account = 'other@x.cn' WHERE id = ?1",
                [ids[0]],
            )
        })
        .unwrap();
    abandon(&state).unwrap();
    let flag = |id: i64| -> i64 {
        state.with_conn(|c| {
            c.query_row("SELECT seen_pending FROM emails WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        })
    };
    assert_eq!(flag(ids[0]), 1, "其它账号的悬挂标志不得被清掉");
    assert_eq!(flag(ids[1]), 0, "当前账号的失败标志应被放弃");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

// ---------- A6/A9（audit 2026-11-16）：清标志失败上报 / 积压自唤醒 ----------

/// A6：清标志失败必须让 flush 返回 Err（调用方 worker 据此退避）。
/// 注入手段：BEFORE UPDATE 触发器 RAISE(ABORT)，专拦 seen_pending 1→0
#[test]
fn flush_reports_clear_pending_failure() {
    let (state, dir, ids) = fixture("a6-clearfail", &[(42, 1), (42, 2)]);
    for id in &ids {
        mark_read(&state, *id).unwrap();
    }
    state
        .with_conn(|c| {
            c.execute_batch(
                "CREATE TRIGGER a6_no_clear BEFORE UPDATE OF seen_pending ON emails
                 WHEN NEW.seen_pending = 0 AND OLD.seen_pending = 1
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
        })
        .unwrap();

    let fetcher = FakeFetcher::ok();
    // 旧实现：mark_seen_batch Ok → clear_pending 静默吞错 → flush Ok(0)，
    // worker failures 恒 0 → 1.5s 无退避自唤醒死循环
    let err = flush(&state, &fetcher).unwrap_err();
    assert!(err.contains("清标志失败"), "错误应带上根因：{err}");
    assert_eq!(
        pending_count(&state),
        2,
        "标志未清（服务器已标已读但本地仍挂起，须退避重试清标志）"
    );
    assert_eq!(fetcher.calls().len(), 1, "STORE 本身已成功发出");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// A6：放弃路径（abandon）清标志失败上报而非吞错
#[test]
fn abandon_reports_clear_failure() {
    let (state, dir, ids) = fixture("a6-abandonfail", &[(42, 1)]);
    mark_read(&state, ids[0]).unwrap();
    state
        .with_conn(|c| {
            c.execute_batch(
                "CREATE TRIGGER a6_no_clear2 BEFORE UPDATE OF seen_pending ON emails
                 WHEN NEW.seen_pending = 0 AND OLD.seen_pending = 1
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
        })
        .unwrap();
    let err = abandon(&state).unwrap_err();
    assert!(err.contains("放弃回写时清标志失败"), "{err}");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// A9：单轮 500 上限之外还有积压时必须自唤醒续轮——不看 done。
/// 旧前置 `done > 0`：首批 500 全命中 ERR_UIDVALIDITY_CHANGED（清标志但
/// done 不增）时，第 501+ 条悬挂到下次点击或重启
#[test]
fn flush_self_wakes_for_backlog_beyond_round_limit() {
    let (state, dir, _) = fixture("a9-backlog", &[(42, 1)]);
    // 直接 SQL 造 600 条待回写行（同一 uidvalidity=42，避免 store_incoming 600 封）
    state.with_conn(|c| {
        for uid in 1000..1600 {
            c.execute(
                "INSERT INTO emails (direction, folder, status, subject, from_addr, to_addrs,
                                      subject_tokens, body_tokens, created_at, remote_uid,
                                      seen_pending, seen_account, unread)
                 VALUES ('in','inbox','received','b','a@x.cn','demo@chinatelecom.cn','','',0,
                         ?1, 1, 'demo@chinatelecom.cn', 0)",
                [format!("42:{uid}")],
            )
            .unwrap();
        }
    });
    assert_eq!(pending_count(&state), 600, "600 条直造待回写行");

    // 自唤醒观测：把 worker 的唤醒口换成测试 channel
    let (tx, rx) = std::sync::mpsc::channel();
    state.set_seen_channel(tx);

    // 全部回写命中 UIDVALIDITY 变化（清标志 + done 不增）
    let fetcher = FakeFetcher::failing(ERR_UIDVALIDITY_CHANGED);
    assert_eq!(
        flush(&state, &fetcher).unwrap(),
        0,
        "UIDVALIDITY 分支不计 done"
    );
    // A9 断言：仍有积压 → 自唤醒被触发一次（旧代码 done==0 短路，这里会失败）
    assert!(
        rx.try_recv().is_ok(),
        "首批 500 清标志后剩余 100 条应触发自唤醒续轮"
    );
    // 第二轮应处理剩余行（500 上限）
    assert_eq!(flush(&state, &fetcher).unwrap(), 0);
    let calls = fetcher.calls();
    assert_eq!(calls[0].4.len(), 500, "第一轮 500 条");
    assert_eq!(calls[1].4.len(), 100, "第二轮恰为剩余 100 条");
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

/// P2-2：标为未读必须清掉打开邮件时写入的 seen_pending，
/// 否则 1.5s 合并窗口到点后 worker 仍会 STORE +\Seen 覆盖用户刚标的未读
#[test]
fn mark_unread_clears_pending_seen_writeback() {
    let (state, dir) = temp_state("p22-unread");
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(direction, folder, status, from_addr, to_addrs, subject,
                                    body_text, unread, created_at, seen_pending, remote_uid)
                 VALUES ('in','inbox','delivered','a@b.cn','me@c.cn','标题','正文',0,?1,1,'7:42')",
                [crate::mail::now_epoch()],
            )
            .unwrap();
        });
    let id: i64 = state.with_conn(|c| {
        c.query_row("SELECT max(id) FROM emails", [], |r| r.get(0)).unwrap()
    });
    assert!(mark_unread(&state, id).unwrap(), "应翻转成功");
    let (unread, pending): (i64, i64) = state.with_conn(|c| {
        c.query_row(
            "SELECT unread, seen_pending FROM emails WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(unread, 1, "本地未读");
    assert_eq!(pending, 0, "必须清掉待回写标志，防 worker 覆盖为服务器已读");
    cleanup(&dir);
}

#[test]
fn p73_reopen_already_read_mail_restores_seen_pending_after_abandon() {
    // 第七轮 P2-3：abandon 清标志后邮件已 unread=0，mark_read 的 AND unread=1
    // 永不命中 → 服务器 \Seen 永久失同步。ensure_seen_recheck 须重建标志并
    // 能被 flush 正常回写。
    let (state, dir, ids) = fixture("p73-recheck", &[(42, 7)]);
    let id = ids[0];
    assert!(mark_read(&state, id).unwrap());
    assert_eq!(pending_count(&state), 1);
    abandon(&state).unwrap();
    assert_eq!(pending_count(&state), 0, "放弃后标志清空");

    // 再次打开已读邮件：mark_read 不再翻转，补偿函数须重建
    assert!(!mark_read(&state, id).unwrap(), "已读邮件不翻转");
    assert_eq!(pending_count(&state), 0, "单靠 mark_read 无法重建");
    assert!(
        ensure_seen_recheck(&state, id).unwrap(),
        "已读行须补回写意向"
    );
    assert_eq!(pending_count(&state), 1);
    // 幂等：重复调用不再重复置位
    assert!(!ensure_seen_recheck(&state, id).unwrap());
    let account: String = state.with_conn(|c| {
        c.query_row("SELECT seen_account FROM emails WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(account, "demo@chinatelecom.cn");

    let fetcher = FakeFetcher::ok();
    assert_eq!(flush(&state, &fetcher).unwrap(), 1, "补偿后须能正常回写");
    assert_eq!(fetcher.calls()[0].4, vec![7]);
    assert_eq!(pending_count(&state), 0);
    state.clear_password("demo@chinatelecom.cn");
    cleanup(&dir);
}

#[test]
fn p73_recheck_skips_sent_and_missing_remote_uid() {
    // 补回写意向只针对收件且有 remote_uid 的行；发件/无 UID 不写服务器的 \Seen
    let (state, dir, ids) = fixture("p73-scope", &[(42, 1)]);
    let id = ids[0];
    mark_read(&state, id).unwrap();
    abandon(&state).unwrap();
    state
        .with_conn(|c| {
            c.execute(
                "UPDATE emails SET direction='out', remote_uid=NULL WHERE id=?1",
                [id],
            )
        })
        .unwrap();
    assert!(!ensure_seen_recheck(&state, id).unwrap(), "发件/无 UID 不得置位");
    assert_eq!(pending_count(&state), 0);
    cleanup(&dir);
}
