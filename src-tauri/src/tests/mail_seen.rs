//! 已读回写批处理单测（src/mail/seen.rs 内联测试，经 #[path] 引入）

use super::*;
use crate::mail::{IncomingMail, LastCursor, mark_read, store_incoming};
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
        attachments: Vec::new(),
        too_large: false,
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
        assert!(mark_read(&state, *id), "首次置已读应生效");
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

/// 网络失败：标志保留（供退避重试），错误向上传递
#[test]
fn flush_keeps_flag_on_transient_failure() {
    let (state, dir, ids) = fixture("fail", &[(42, 7)]);
    mark_read(&state, ids[0]);
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
    mark_read(&state, ids[0]);
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
        mark_read(&state, *id);
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
        mark_read(&state, *id);
    }
    assert_eq!(pending_count(&state), 2);
    abandon(&state);
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
            subject: "草稿".into(),
            body_text: "体".into(),
            in_reply_to: None,
        },
    )
    .unwrap();
    mark_read(&state, id); // 草稿 unread=0：本来就不翻转
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
    assert!(mark_read(&state, valid_id), "正常行应置为待回写");

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
