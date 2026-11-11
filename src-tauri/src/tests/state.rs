//! state 模块单元测试（自 src/state.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::conf::save;
use crate::db::{open, settings};
use crate::mail::{now_epoch, save_draft};

/// 测试便捷：读取成功即展开（错误路径见 tests/conf.rs 的 Err 断言）
fn load(p: &Path) -> std::collections::BTreeMap<String, String> {
    crate::conf::load(p).expect("配置读取失败")
}

#[test]
fn first_fetch_days_parses_full_zero() {
    // 回归：选"全部"（配置=0）曾因 .filter(|v| *v>0) 被吞成默认 7 天，只拉近一周。
    // 0 是合法值 = 全部（UID SEARCH ALL），必须原样透传给 fetch。
    assert_eq!(first_fetch_days_from_raw("0"), 0, "0=全部，不得被过滤");
    assert_eq!(first_fetch_days_from_raw("7"), 7);
    assert_eq!(first_fetch_days_from_raw("30"), 30);
    assert_eq!(first_fetch_days_from_raw("90"), 90);
    // 解析失败/未配置 → 兜底默认 30（与 defaults 一致）
    assert_eq!(first_fetch_days_from_raw(""), 30);
    assert_eq!(first_fetch_days_from_raw("abc"), 30);
}

#[test]
fn fetch_watchdog_resets_hung_round() {
    let dir = temp_dir("wd");
    let state = AppState::open(&dir);
    // 模拟挂起轮：running 且 started_at 拨回 6 分钟前（超出看门狗 300s）
    {
        let mut s = state.fetch_state.lock().unwrap();
        s.running = true;
        s.epoch = 7;
        s.started_at = now_epoch() - (FETCH_WATCHDOG_SECS + 60);
    }
    let snap = state.fetch_snapshot();
    assert!(!snap.running, "看门狗应复位挂起轮");
    assert!(snap.last_error.contains("收件超时"), "应给出超时原因");
    assert!(snap.just_finished, "复位应触发一次完成刷新");
    assert_eq!(
        snap.epoch, 8,
        "复位应推进 epoch（挂起线程的迟到 finish 将被丢弃）"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fetch_late_finish_discarded_by_epoch() {
    let dir = temp_dir("wd-late");
    let state = AppState::open(&dir);
    // 看门狗已复位（epoch 3），挂起线程的 epoch=2 finish 不得写状态
    state.fetch_end(2, 99, "迟到的失败");
    let snap = state.fetch_snapshot();
    assert_eq!(snap.new_count, 0, "迟到结果必须丢弃");
    assert!(!snap.last_error.contains("迟到"), "迟到错误不得污染展示");
    assert!(!snap.just_finished);
    std::fs::remove_dir_all(&dir).unwrap();
}

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
    let id = save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "迁移用例".into(),
            body_text: "正文".into(),
            in_reply_to: None,
            scheduled_at: 0,
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
    let moved_db = dest.join("corpmail.db");
    let conn = open(&moved_db).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "快照应含全部邮件");
    // 配置不落库（验收拍板）：库无可配置键，配置随 corpmail.toml 常驻锚点
    assert!(
        settings::get(&conn, keys::ACCOUNT_PREFIX)
            .unwrap()
            .is_none(),
        "配置键不得入库（迁移库当然也没有）"
    );
    let cfg_after = load(&anchor);
    assert_eq!(
        cfg_after.get(keys::ACCOUNT_PREFIX).map(String::as_str),
        Some("demo"),
        "配置应在锚点 corpmail.toml（数据目录变更不影响）"
    );
    assert!(
        dest.join(format!("attachments/2026-10/{id}/1_a.bin.gz"))
            .exists(),
        "附件文件应复制"
    );
    // 指针仅写独立配置文件（不落库，验收拍板简化）；启动解析定位新目录
    let anchor_conn = open(&anchor.join("corpmail.db")).unwrap();
    assert!(
        settings::get(&anchor_conn, keys::DATA_DIR)
            .unwrap()
            .is_none(),
        "迁移指针不得写库（corpmail.toml 为唯一持久源）"
    );
    let cfg = load(&anchor);
    assert_eq!(
        cfg.get(keys::DATA_DIR).map(String::as_str),
        Some(dest.to_str().unwrap()),
        "指针应写独立配置文件 corpmail.toml"
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
    // 已有 corpmail.db 的目录拒绝
    std::fs::write(other.join("corpmail.db"), b"exists").unwrap();
    assert!(
        state.move_data_dir(other.to_str().unwrap()).is_err(),
        "占用目录拒绝"
    );
    // 相对路径拒绝
    assert!(state.move_data_dir("relative/dir").is_err());
    // 失败不应留下指针（配置文件与库镜像均不得写入）
    let conn = open(&cur.join("corpmail.db")).unwrap();
    assert!(
        settings::get(&conn, keys::DATA_DIR).unwrap().is_none(),
        "失败不得写库指针"
    );
    drop(conn);
    assert!(
        !load(&cur).contains_key(keys::DATA_DIR),
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
    // 验收反馈场景：迁移后用户删除锚点目录 corpmail.db —— 指针在独立配置文件
    // corpmail.toml 中，删除库不影响定位，不误判首次启动
    let base = temp_dir("resolve-anchor-del");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    // 造一封邮件：验证定位后数据完整（非空壳）
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "兜底定位用例".into(),
            body_text: "正文".into(),
            in_reply_to: None,
            scheduled_at: 0,
        },
    )
    .unwrap();
    let dest = base.join("moved");
    state.move_data_dir(dest.to_str().unwrap()).unwrap();
    drop(state);
    // 迁移后：锚点保留兼容镜像库 + 独立配置文件（配置不随数据迁移）
    assert!(anchor.join("corpmail.db").is_file(), "锚点兼容镜像库存在");
    assert!(
        anchor.join(crate::conf::CONFIG_FILE).is_file(),
        "独立配置文件已写入"
    );
    // 用户删除锚点库 —— 定位不受影响
    std::fs::remove_file(anchor.join("corpmail.db")).unwrap();
    assert_eq!(
        AppState::resolve_start_dir(&anchor),
        dest,
        "锚点库被删后应经配置文件继续定位自定义目录（不误判首次启动）"
    );
    // 定位到的库可正常打开且数据完整（非空壳）
    let conn = open(&dest.join("corpmail.db")).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "自定义目录数据完整");
    // 模拟重启后（数据目录已在新位置）：不复活锚点库、无待生效提示
    let state2 = AppState::open_with_anchor(&dest, &anchor);
    assert!(
        !anchor.join("corpmail.db").exists(),
        "open 不得复活锚点库文件"
    );
    assert_eq!(
        state2.pending_data_dir(),
        None,
        "指针已生效（==当前目录）应无待生效提示"
    );
    // 配置指针也失效（指向已删目录）→ 回退锚点
    let mut cfg = load(&anchor);
    cfg.insert(keys::DATA_DIR.to_string(), "/nonexistent/usb-gone".into());
    save(&anchor, &cfg).unwrap();
    assert_eq!(
        AppState::resolve_start_dir(&anchor),
        anchor,
        "指针失效应回退锚点"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn pending_data_dir_reads_config_file_after_db_delete() {
    // 未重启状态下删锚点库：待生效目录从 corpmail.toml 读出（迁移待生效仍可见）
    let base = temp_dir("pending-conf");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    let dest = base.join("moved");
    state.move_data_dir(dest.to_str().unwrap()).unwrap();
    drop(state);
    std::fs::remove_file(anchor.join("corpmail.db")).unwrap();
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
    // 有效性加强：配置指针指向存在但无 corpmail.db 的目录（空壳）→ 不采纳，回退锚点
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
    save(&anchor, &cfg).unwrap();
    assert_eq!(
        AppState::resolve_start_dir(&anchor),
        anchor,
        "空壳目录不得被当作有效数据目录"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn config_keys_stored_in_toml_only() {
    // 配置键仅写 corpmail.toml（唯一持久源，验收拍板简化）：不落库、get 直读回；
    // 非配置键仅入库；配置文件丢失后无库镜像可回填（by-design：需重新配置）
    let base = temp_dir("conf-solo");
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
    let cfg = load(&dir);
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
    // 库中无配置键（验收拍板：DB 不保存 corpmail.toml 已有配置）
    {
        let conn = open(&dir.join("corpmail.db")).unwrap();
        assert!(
            settings::get(&conn, keys::SMTP_HOST).unwrap().is_none(),
            "配置键不得写库"
        );
        assert_eq!(
            settings::get(&conn, keys::KEEP_INBOX_DAYS)
                .unwrap()
                .as_deref(),
            Some("60"),
            "非配置键正常入库"
        );
    }
    drop(state);
    // 配置文件丢失：无镜像可回填（唯一持久源语义），重新打开后未配置
    std::fs::remove_file(dir.join(crate::conf::CONFIG_FILE)).unwrap();
    AppState::open(&dir);
    assert!(
        !dir.join(crate::conf::CONFIG_FILE).is_file(),
        "无库镜像时不得凭空重建配置文件"
    );
    let state2 = AppState::open(&dir);
    assert!(
        state2.get(keys::SMTP_HOST).unwrap().is_none(),
        "配置文件丢失后读不到配置（需重新配置）"
    );
    assert!(!state2.configured(), "应回到未配置态");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn delete_old_backup_removes_files_but_keeps_config() {
    // 迁移后（重启生效态）：删除锚点备份 —— 数据/定位不受影响。
    // 注意 fixture 必须有邮件：启动自动清理会删掉空的遗留库
    //（try_auto_clean_empty_old_backup），无邮件的话备份活不到手动删除那一步
    let base = temp_dir("backup-del");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "备份保留用例".into(),
            body_text: "正文".into(),
            in_reply_to: None,
            scheduled_at: 0,
        },
    )
    .unwrap();
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
    assert!(!anchor.join("corpmail.db").exists(), "锚点库应被删除");
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
    assert!(dest.join("corpmail.db").is_file(), "当前数据完好");
    // 重复删除 → 无备份可删
    assert!(state2.delete_old_backup().is_err());
    let _ = std::fs::remove_dir_all(&base);
}

/// 迁移重启后：旧目录遗留库为空（仅配置、无邮件/附件）→ 启动自动清理，
/// 免去存储页手动删除；配置文件与新目录数据不受影响
#[test]
fn auto_clean_removes_empty_old_backup_after_restart() {
    let base = temp_dir("auto-clean-empty");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    // 空库：仅配置行（典型：装好首设后立即改目录，还没收到任何邮件）
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let dest = base.join("moved");
    state.move_data_dir(dest.to_str().unwrap()).unwrap();
    drop(state);
    // 迁移后未重启：本进程还持有旧库，自动清理不得在迁移会话内提前发生
    //（该断言同时验证清理只在重启后触发）
    // 模拟重启：新目录生效，启动链自动清理空遗留
    let state2 = AppState::open_with_anchor(&dest, &anchor);
    assert!(
        !anchor.join("corpmail.db").exists(),
        "空遗留库应在重启后被自动清理"
    );
    assert!(
        !anchor.join("corpmail.db-wal").exists() && !anchor.join("corpmail.db-shm").exists(),
        "遗留 wal/shm 一并清理"
    );
    assert!(!anchor.join("attachments").exists(), "空附件目录一并清理");
    assert!(
        anchor.join(crate::conf::CONFIG_FILE).is_file(),
        "配置文件必须保留（指针仍指向新目录）"
    );
    assert!(
        state2.old_backup_stats().is_none(),
        "清理后存储页不再显示迁移备份"
    );
    assert!(dest.join("corpmail.db").is_file(), "新目录数据完好");
    assert_eq!(
        AppState::resolve_start_dir(&anchor),
        dest,
        "定位不受清理影响"
    );
    // 幂等：再次重启不误删任何东西（无遗留可清）
    drop(state2);
    AppState::open_with_anchor(&dest, &anchor);
    assert!(dest.join("corpmail.db").is_file());
    let _ = std::fs::remove_dir_all(&base);
}

/// 迁移重启后：旧目录遗留库有邮件 → 自动清理跳过，备份保留给用户手动删
#[test]
fn auto_clean_keeps_backup_when_old_db_has_mails() {
    let base = temp_dir("auto-clean-keep");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "张伟".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "遗留数据".into(),
            body_text: "正文".into(),
            in_reply_to: None,
            scheduled_at: 0,
        },
    )
    .unwrap();
    let dest = base.join("moved");
    state.move_data_dir(dest.to_str().unwrap()).unwrap();
    drop(state);
    let state2 = AppState::open_with_anchor(&dest, &anchor);
    assert!(
        anchor.join("corpmail.db").is_file(),
        "有邮件的遗留库不得被自动清理"
    );
    assert!(
        state2.old_backup_stats().is_some(),
        "存储页仍应显示迁移备份"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// 迁移后未重启：本进程仍直连旧库，删旧备份会让后续写入落进已 unlink 的文件
/// → 重启后只剩迁移时快照，期间变更全丢。必须拒绝（审计 P2-7）
#[test]
fn delete_old_backup_refuses_before_restart() {
    let base = temp_dir("backup-pending");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    let dest = base.join("moved");
    state.move_data_dir(dest.to_str().unwrap()).unwrap();
    assert!(
        state.pending_data_dir().is_some(),
        "未重启时指针应处于待生效态"
    );
    let err = state.delete_old_backup().unwrap_err();
    assert!(err.contains("尚未重启生效"), "应提示先重启：{err}");
    assert!(anchor.join("corpmail.db").is_file(), "拒绝时不得删除旧库");
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

#[test]
fn persist_fetch_cursor_advances_monotonically_and_resets_on_uidvalidity_change() {
    // 需求2 断点续传 + C4 并发加固：每批入库后立即持久化游标；
    // 单调性以 DB 侧现值为准（不看内存快照），uidvalidity 变化无条件采纳。
    let dir = temp_dir("cursor");
    let state = AppState::open(&dir);
    let read_uid = |state: &AppState| {
        state
            .with_conn(|c| settings::get(c, keys::LAST_UID))
            .unwrap()
    };
    let read_uv = |state: &AppState| {
        state
            .with_conn(|c| settings::get(c, keys::LAST_UIDVALIDITY))
            .unwrap()
    };
    let push = |state: &AppState, uv: u32, uid: u32, sync: bool| {
        state.with_conn(|conn| {
            persist_fetch_cursor(conn, keys::LAST_UIDVALIDITY, keys::LAST_UID, uv, uid, sync)
        })
    };

    // 1) 首拉后首个批次：uidvalidity + uid 落库；sync=false 不写 last_sync_at
    push(&state, 1, 100, false).unwrap();
    assert_eq!(read_uv(&state).as_deref(), Some("1"));
    assert_eq!(read_uid(&state).as_deref(), Some("100"));
    assert!(
        state
            .with_conn(|c| settings::get(c, keys::LAST_SYNC_AT))
            .unwrap()
            .is_none(),
        "非收尾调用不得写 last_sync_at"
    );

    // 2) 同空间下一批 uid 更大：推进
    push(&state, 1, 150, false).unwrap();
    assert_eq!(read_uid(&state).as_deref(), Some("150"));

    // 3) 单调不回退：uid 更小（SINCE 兜底命中可能低于已推进值）不得改写
    push(&state, 1, 120, false).unwrap();
    assert_eq!(read_uid(&state).as_deref(), Some("150"));

    // 4) C4 竞态模拟：无视内存状态的"旧轮迟写"——DB 已推进到 200，
    //    旧轮携带过期快照值 150 写入也不得回退（DB 侧单调）
    push(&state, 1, 200, false).unwrap();
    push(&state, 1, 150, false).unwrap(); // 旧轮迟到写
    assert_eq!(read_uid(&state).as_deref(), Some("200"));

    // 5) 空批（uid=0）：只采纳 uidvalidity，uid 保持上一值
    push(&state, 2, 0, false).unwrap();
    assert_eq!(read_uv(&state).as_deref(), Some("2"));
    assert_eq!(read_uid(&state).as_deref(), Some("200"));

    // 6) uidvalidity 变化（UID 空间重置）：无条件采纳新空间，uid 允许变小
    push(&state, 3, 5, false).unwrap();
    assert_eq!(read_uv(&state).as_deref(), Some("3"));
    assert_eq!(read_uid(&state).as_deref(), Some("5"));

    // 7) 收尾（sync=true）：补写 last_sync_at
    push(&state, 3, 5, true).unwrap();
    assert!(
        state
            .with_conn(|c| settings::get(c, keys::LAST_SYNC_AT))
            .unwrap()
            .is_some(),
        "收尾调用应写 last_sync_at"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)] // chmod 只读目录仅在 Unix 可靠（C6 清理回归）
#[test]
fn move_data_dir_cleans_residue_and_restores_pointer_on_save_failure() {
    // C6：迁移中途失败必须清目标残留（快照/附件副本）并回滚内存镜像指针，
    // 否则向导重试同目录被"目标已存在 corpmail.db"拦死，形成无出口的半保存态
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mig-fail");
    let state = AppState::open(&dir);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    let target = std::env::temp_dir().join(format!("mbx-mig-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);

    // 锚点目录置只读 → conf::save（写 corpmail.toml.tmp）失败
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&dir, perms).unwrap();
    let res = state.move_data_dir(target.to_str().unwrap());
    // 恢复可写（后续清理依赖）
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dir, perms).unwrap();

    let err = res.expect_err("锚点只读时配置写入应失败");
    assert!(
        err.contains("配置文件写入失败"),
        "错误信息应指明配置写入：{err}"
    );
    // R2：失败必须解冻，否则 UI 会停在"请重启"却并没有完成迁移
    assert!(
        !state.migration_pending(),
        "迁移失败必须清除冻结位，让用户继续用旧目录"
    );
    assert!(
        !target.join("corpmail.db").exists(),
        "失败后必须清理目标快照残留，否则重试被拦（C6）"
    );
    assert!(
        state.get(keys::DATA_DIR).unwrap().is_none(),
        "失败后内存镜像指针必须回滚（镜像须与文件一致）"
    );
    // 失败后原地重试：目标已无残留，锚点恢复可写 → 应成功
    state
        .move_data_dir(target.to_str().unwrap())
        .expect("重试应成功（无残留 + 可写）");
    assert!(
        target.join("corpmail.db").exists(),
        "重试成功后目标应含快照库"
    );
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 目标目录已有 attachments/ → 拒绝迁移（避免合并后回滚整目录删除误伤用户文件）
#[test]
fn move_data_dir_rejects_existing_attachments() {
    let base = temp_dir("move-att-exists");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    let dest = base.join("dest");
    std::fs::create_dir_all(dest.join("attachments")).unwrap();
    std::fs::write(dest.join("attachments/user-file.txt"), b"keep").unwrap();

    let err = state.move_data_dir(dest.to_str().unwrap()).unwrap_err();
    assert!(err.contains("attachments"), "应指出冲突来源：{err}");
    assert!(
        dest.join("attachments/user-file.txt").is_file(),
        "拒绝迁移时不得触碰目标目录既有文件"
    );
    assert!(!dest.join("corpmail.db").exists(), "拒绝时不应产生快照库");
    let _ = std::fs::remove_dir_all(&base);
}

/// 指针写入失败 → 回滚只删本次产出物：目标目录里用户既有文件必须原样保留
/// （旧实现对 dest/attachments 直接 remove_dir_all，属不可逆误删）
#[test]
fn move_data_dir_rollback_spares_preexisting_files() {
    let base = temp_dir("move-rollback");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    let dest = base.join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("notes.txt"), b"keep-me").unwrap();
    // 让配置写入必然失败：原子保存的临时文件路径被同名目录占位
    // （tmp 名带 pid，见 conf::save，故按同格式构造）
    std::fs::create_dir_all(anchor.join(format!(
        "{}.{}.tmp",
        crate::conf::CONFIG_FILE,
        std::process::id()
    )))
    .unwrap();

    let err = state.move_data_dir(dest.to_str().unwrap()).unwrap_err();
    assert!(
        err.contains("配置文件写入失败"),
        "应停在指针写入步骤：{err}"
    );
    assert!(!dest.join("corpmail.db").exists(), "回滚应删除本次快照库");
    assert_eq!(
        std::fs::read(dest.join("notes.txt")).unwrap(),
        b"keep-me",
        "回滚不得删除目标目录既有文件"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn start_fetch_skips_when_unconfigured() {
    // 回归：启动 2s 首收跑在向导之前，未拦截时会以空账号写一条
    // 「凭据缺失」假错误——用户完成首配后状态条立刻显示这条陈旧错误
    let dir = temp_dir("fetch-unconfigured");
    let state = AppState::open(&dir);
    state.start_fetch();
    let snap = state.fetch_snapshot();
    assert!(!snap.running, "未配置不得开轮");
    assert!(
        snap.last_error.is_empty(),
        "未配置不得写错误：{}",
        snap.last_error
    );
    assert_eq!(snap.epoch, 0, "未配置不得推进代际");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 运行中重复触发（定时/页面重复请求）应被忽略：不开启新轮、不改变代际
#[test]
fn start_fetch_ignored_while_running() {
    let dir = temp_dir("fetch-ignore");
    let state = AppState::open(&dir);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    {
        let mut s = state.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
        s.running = true;
        s.epoch = 7;
    }
    state.start_fetch();
    let s = state.fetch_state.lock().unwrap_or_else(|e| e.into_inner());
    assert!(s.running, "运行中应保持运行");
    assert_eq!(s.epoch, 7, "运行中重复触发不得开启新轮（epoch 不变）");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 收件名额上限：正常轮打满 → 拒绝并说明；被判死的轮打满 → 提示重启
#[test]
fn fetch_refuses_new_round_at_hang_cap() {
    let dir = temp_dir("fetch-cap");
    let state = AppState::open(&dir);
    // 正常轮打满（并发 4）：本次跳过，说明原因
    state.active_rounds.store(
        MAX_CONCURRENT_FETCH_ROUNDS,
        std::sync::atomic::Ordering::SeqCst,
    );
    state.start_fetch();
    let snap = state.fetch_snapshot();
    assert!(!snap.running, "达到上限时不得开新轮");
    assert!(
        snap.last_error.contains("后台运行"),
        "应说明拒绝原因：{}",
        snap.last_error
    );
    // 已被看门狗判死（detached，线程可能永不返回）的轮打满：只能重启
    state
        .active_rounds
        .store(0, std::sync::atomic::Ordering::SeqCst);
    state.detached_rounds.store(
        MAX_DETACHED_FETCH_ROUNDS,
        std::sync::atomic::Ordering::SeqCst,
    );
    state.start_fetch();
    let snap = state.fetch_snapshot();
    assert!(!snap.running, "detached 上限同样拒绝");
    assert!(
        snap.last_error.contains("后台无响应"),
        "应提示重启：{}",
        snap.last_error
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 单封收件上限：解析失败/缺省取 50MB，越界钳制到 1..=200MB（0 会让所有邮件成占位）
#[test]
fn max_mail_bytes_clamps_to_reasonable_range() {
    assert_eq!(max_mail_bytes_from("50"), 50 * 1024 * 1024);
    assert_eq!(max_mail_bytes_from(""), 50 * 1024 * 1024, "缺省 50MB");
    assert_eq!(max_mail_bytes_from("abc"), 50 * 1024 * 1024, "非法值兜底");
    assert_eq!(
        max_mail_bytes_from("0"),
        MAX_MAIL_MB_MIN as u64 * 1024 * 1024
    );
    assert_eq!(
        max_mail_bytes_from("9999"),
        MAX_MAIL_MB_MAX as u64 * 1024 * 1024
    );
    assert_eq!(max_mail_bytes_from(" 1 "), 1024 * 1024);
}

/// 已读回写唤醒：装配通道后 notify 立即送达；未装配时为空操作（不 panic）
#[test]
fn notify_seen_delivers_when_channel_installed() {
    let dir = std::env::temp_dir().join(format!("corpmail-state-seen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = AppState::open(&dir);
    // 未装配：静默忽略
    state.notify_seen();
    let (tx, rx) = std::sync::mpsc::channel();
    state.set_seen_channel(tx);
    state.notify_seen();
    assert!(rx.try_recv().is_ok(), "装配后应立即收到唤醒");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 重试前重读游标：写游标后 read_fetch_cursor 应读回（断点续传依据）
#[test]
fn read_fetch_cursor_reads_persisted_values() {
    let dir = temp_dir("read-cursor");
    let state = AppState::open(&dir);
    let mut conn = open(&state.data_dir().join("corpmail.db")).unwrap();
    persist_fetch_cursor(
        &mut conn,
        keys::LAST_UIDVALIDITY,
        keys::LAST_UID,
        42,
        1000,
        false,
    )
    .unwrap();
    let cur = read_fetch_cursor(&conn, keys::LAST_UIDVALIDITY, keys::LAST_UID);
    assert_eq!((cur.uidvalidity, cur.uid), (42, 1000));
    // 未写游标：全 0，不影响增量判断
    let dir2 = temp_dir("read-cursor-empty");
    let state2 = AppState::open(&dir2);
    let conn2 = open(&state2.data_dir().join("corpmail.db")).unwrap();
    let empty = read_fetch_cursor(&conn2, keys::LAST_UIDVALIDITY, keys::LAST_UID);
    assert_eq!((empty.uidvalidity, empty.uid), (0, 0));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// 重试退避：15s 起线性增长（给服务器限流/网络抖动留恢复窗口）
#[test]
fn fetch_retry_delay_grows_linearly() {
    assert_eq!(fetch_retry_delay(1), 15);
    assert_eq!(fetch_retry_delay(2), 30);
}

#[test]
fn switch_data_dir_first_run_swaps_in_place_without_restart() {
    // 需求回归：首设向导选非默认目录不得走迁移——空库直切，会话内立即生效。
    let anchor = temp_dir("switch-anchor");
    let state = AppState::open_with_anchor(&anchor, &anchor);
    assert!(state.store_is_empty(), "新库应为空");
    let target = std::env::temp_dir().join(format!("mbx-switch-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);

    let msg = state
        .switch_data_dir_first_run(target.to_str().unwrap())
        .unwrap();
    assert!(msg.contains("已设置"), "成功信息应说明已设置：{msg}");
    // 会话内切换：路径、库、指针全部就位，无"待重启"状态
    assert_eq!(state.data_dir(), target);
    assert!(target.join("corpmail.db").exists());
    assert!(state.pending_data_dir().is_none());
    assert_eq!(
        load(&anchor).get(keys::DATA_DIR).map(String::as_str),
        Some(target.to_str().unwrap()),
        "锚点 corpmail.toml 指针应指向新目录"
    );
    // 旧默认目录（== 锚点）的启动空库应被清理
    assert!(!anchor.join("corpmail.db").exists());

    // 后续配置写入新库（这正是"直切先于落库"的意义）
    state
        .set_batch(&[
            (keys::ACCOUNT_PREFIX, "demo"),
            (keys::FIRST_FETCH_DAYS, "90"),
        ])
        .unwrap();
    assert!(state.configured());
    assert_eq!(
        state.get_or(keys::FIRST_FETCH_DAYS, &defaults()).as_deref(),
        Some("90")
    );
    // 重启视角：从锚点解析指针 → 新目录，配置完好
    let reopened = AppState::open_with_anchor(&AppState::resolve_start_dir(&anchor), &anchor);
    assert_eq!(reopened.data_dir(), target);
    assert!(reopened.configured());
    assert_eq!(
        reopened
            .get_or(keys::FIRST_FETCH_DAYS, &defaults())
            .as_deref(),
        Some("90")
    );
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_dir_all(&anchor);
}

#[test]
fn switch_data_dir_first_run_refuses_nonempty_store_and_bad_targets() {
    // 安全阀：库里有任何用户痕迹都拒绝直切（宁可走迁移），防误切丢数据
    let anchor = temp_dir("switch-guard");
    let state = AppState::open_with_anchor(&anchor, &anchor);
    save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: "测试用户".into(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "草稿".into(),
            body_text: "正文".into(),
            in_reply_to: None,
            scheduled_at: 0,
        },
    )
    .unwrap();
    let target = std::env::temp_dir().join(format!("mbx-switch-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    let err = state
        .switch_data_dir_first_run(target.to_str().unwrap())
        .unwrap_err();
    assert!(err.contains("非空"), "非空库应拒绝直切：{err}");
    assert_eq!(state.data_dir(), anchor, "拒绝后路径不得变化");
    assert!(
        !target.join("corpmail.db").exists(),
        "拒绝时不得在新目录建库"
    );
    let _ = std::fs::remove_dir_all(&target);

    // 目标非法：相对路径 / 与当前目录互相嵌套 / 已有库文件。
    // 校验分支须在"空库"上验证——非空安全阀先于路径校验，测不到后面
    let anchor2 = temp_dir("switch-guard2");
    let state = AppState::open_with_anchor(&anchor2, &anchor2);
    assert!(state.switch_data_dir_first_run("relative/dir").is_err());
    let err = state
        .switch_data_dir_first_run(anchor2.join("sub").to_str().unwrap())
        .unwrap_err();
    assert!(
        err.contains("嵌套") || err.contains("位于"),
        "嵌套目录应拒绝：{err}"
    );

    let occupied = temp_dir("switch-occupied");
    open(&occupied.join("corpmail.db")).unwrap();
    let err = state
        .switch_data_dir_first_run(occupied.to_str().unwrap())
        .unwrap_err();
    assert!(err.contains("corpmail.db"), "已有库文件的目标应拒绝：{err}");
    let _ = std::fs::remove_dir_all(&occupied);
    let _ = std::fs::remove_dir_all(&anchor2);
    let _ = std::fs::remove_dir_all(&anchor);
}

#[test]
fn rebind_send_worker_rebuilds_on_new_dir() {
    // 发送 worker 持有旧库连接与旧附件根；直切后必须重绑到新目录，
    // 否则发送线程会把待发邮件读旧库、附件读旧目录
    let anchor = temp_dir("rebind-anchor");
    let state = AppState::open_with_anchor(&anchor, &anchor);
    state.start_send_worker(); // 模拟 lib.rs 装配（旧 worker 绑旧目录）
    let target = std::env::temp_dir().join(format!("mbx-rebind-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    state
        .switch_data_dir_first_run(target.to_str().unwrap())
        .unwrap();
    let bound = state
        .send_tx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    assert!(bound, "直切后发送 worker 应已在新目录重建");
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_dir_all(&anchor);
}

/// 首拉阶段化标志（发件箱优先）：读写口径与"未配置即未完成"语义
#[test]
fn first_pull_done_flags_read_and_mark() {
    let dir = std::env::temp_dir().join(format!("corpmail-st-flags-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = open(&dir.join("corpmail.db")).unwrap();
    // 未配置 → 未完成（false）：阶段化模式在全新库上必然开启
    assert!(!first_pull_done(&conn, crate::mail::MailDest::Sent));
    assert!(!first_pull_done(&conn, crate::mail::MailDest::Inbox));
    // 置位后 → 完成；两箱标志互不影响
    mark_first_pull_done(&conn, crate::mail::MailDest::Sent);
    assert!(first_pull_done(&conn, crate::mail::MailDest::Sent));
    assert!(!first_pull_done(&conn, crate::mail::MailDest::Inbox));
    mark_first_pull_done(&conn, crate::mail::MailDest::Inbox);
    assert!(first_pull_done(&conn, crate::mail::MailDest::Inbox));
    // 范围重拉语义：清零后回到未完成（refetch 路径写 "0"）
    settings::set(&conn, keys::FIRST_PULL_DONE_SENT, "0").unwrap();
    assert!(!first_pull_done(&conn, crate::mail::MailDest::Sent));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- 发件显示名自动推断（配置留空 → 发件箱 From 补齐） ----------

fn insert_sent_mail(conn: &Connection, from_addr: &str, from_name: &str, at: i64) {
    conn.execute(
        "INSERT INTO emails(direction, folder, status, subject, from_addr, from_name,
                            to_addrs, subject_tokens, body_tokens, attach_cnt, attach_bytes,
                            unread, created_at, sent_at)
         VALUES('out', 'sent', 'sent', 't', ?1, ?2, 'someone@example.com',
                '', '', 0, 0, 0, ?3, ?3)",
        rusqlite::params![from_addr, from_name, at],
    )
    .unwrap();
}

fn configured_state(dir: &Path, display: &str) -> AppState {
    let s = AppState::open(dir);
    s.set_batch(&[
        (keys::ACCOUNT_PREFIX, "demo"),
        (keys::ACCOUNT_DOMAIN, "@chinatelecom.cn"),
        (keys::DISPLAY_NAME, display),
    ])
    .unwrap();
    s
}

fn settings_rows(s: &AppState) -> i64 {
    s.with_conn(|c| {
        c.query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
            .unwrap()
    })
}

#[test]
fn display_name_prefers_latest_sent_name_and_writes_no_marker() {
    let dir = temp_dir("display-infer");
    let s = configured_state(&dir, "");
    s.with_conn(|c| {
        insert_sent_mail(c, "demo@chinatelecom.cn", "张伟", 100);
        insert_sent_mail(c, "demo@chinatelecom.cn", "张伟", 200);
        insert_sent_mail(c, "demo@chinatelecom.cn", "张伟", 300);
        // 改名后发出的邮件时间最新：应取新名，不因旧名出现 3 次被压过
        insert_sent_mail(c, "demo@chinatelecom.cn", "张小伟", 400);
        // 收件箱里"对方显示名"绝不能参与推断
        c.execute(
            "INSERT INTO emails(direction, folder, status, subject, from_addr, from_name,
                                to_addrs, subject_tokens, body_tokens, attach_cnt, attach_bytes,
                                unread, created_at, received_at)
             VALUES('in', 'inbox', 'received', 't', 'other@example.com', '外人',
                    'demo@chinatelecom.cn', '', '', 0, 0, 0, 500, 500)",
            [],
        )
        .unwrap();
    });
    let before = settings_rows(&s);
    assert_eq!(s.display_name(), "张小伟", "用户改名后以时间最新的发件为准");
    // 写回 corpmail.toml（配置唯一持久源），且重复读取稳定
    let cfg = load(&dir);
    assert_eq!(
        cfg.get(keys::DISPLAY_NAME).map(String::as_str),
        Some("张小伟")
    );
    // 自动推断不写标记：标记只属于「用户主动清空」，否则错误结果会被永久固化
    assert_eq!(
        cfg.get(keys::DISPLAY_NAME_AUTO),
        None,
        "自动推断不得写 display_name_auto"
    );
    assert_eq!(s.display_name(), "张小伟");
    // 不得写 settings 表：store_is_empty() 以其行数判定首设空库
    assert_eq!(settings_rows(&s), before, "自动推断不得污染 settings 表");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_name_not_overwritten_when_configured() {
    let dir = temp_dir("display-keep");
    let s = configured_state(&dir, "李雷");
    s.with_conn(|c| insert_sent_mail(c, "demo@chinatelecom.cn", "张伟", 100));
    assert_eq!(s.display_name(), "李雷", "用户显式设置的显示名不得被覆盖");
    assert_eq!(
        load(&dir).get(keys::DISPLAY_NAME_AUTO),
        None,
        "未发生推断不应写标记"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_name_not_refilled_after_user_clears() {
    let dir = temp_dir("display-clear");
    let s = configured_state(&dir, "");
    s.with_conn(|c| insert_sent_mail(c, "demo@chinatelecom.cn", "张伟", 100));
    assert_eq!(s.display_name(), "张伟", "前置：先自动推断成功");
    // 与设置·账号页 settings_account_save 同口径：清空名字时写入防回填标记
    s.set_batch(&[(keys::DISPLAY_NAME, ""), (keys::DISPLAY_NAME_AUTO, "1")])
        .unwrap();
    assert_eq!(s.display_name(), "", "主动清空后不得被自动回填");
    assert_eq!(
        load(&dir).get(keys::DISPLAY_NAME).map(String::as_str),
        Some("")
    );
    assert_eq!(
        load(&dir).get(keys::DISPLAY_NAME_AUTO).map(String::as_str),
        Some("1")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_name_reinfers_after_user_refills_and_clears_marker() {
    // 错误推断不被永久固化：用户重新填写名字会清标记，此后清空名字可重新推断
    let dir = temp_dir("display-reinfer");
    let s = configured_state(&dir, "");
    s.with_conn(|c| insert_sent_mail(c, "demo@chinatelecom.cn", "张伟", 100));
    assert_eq!(s.display_name(), "张伟");
    s.set_batch(&[(keys::DISPLAY_NAME, "李雷"), (keys::DISPLAY_NAME_AUTO, "")])
        .unwrap();
    assert_eq!(
        load(&dir).get(keys::DISPLAY_NAME_AUTO).map(String::as_str),
        Some(""),
        "重新填写名字后应清掉防回填标记"
    );
    s.set_batch(&[(keys::DISPLAY_NAME, "")]).unwrap();
    s.invalidate_display_name_probe();
    // 新到的发件箱邮件带改名后的姓名，重新推断应立即生效
    s.with_conn(|c| insert_sent_mail(c, "demo@chinatelecom.cn", "王芳", 900));
    assert_eq!(s.display_name(), "王芳", "标记清空后应重新取最新姓名");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_name_skips_invalid_candidates_and_breaks_ties_by_time() {
    let dir = temp_dir("display-clean");
    let s = configured_state(&dir, "");
    s.with_conn(|c| {
        // 出现次数最多的候选是乱码，须整组跳过而不是直接采纳
        for i in 0..5 {
            insert_sent_mail(c, "demo@chinatelecom.cn", "ä½\u{a0}å¥½", 10 + i);
        }
        // 被塞成邮箱地址 / 带 CR LF 头注入
        insert_sent_mail(c, "demo@chinatelecom.cn", "demo@chinatelecom.cn", 20);
        insert_sent_mail(c, "demo@chinatelecom.cn", "张伟\r\nX-Injected: 1", 30);
        // 两个合法候选各 1 次：并列取时间最新，引号包裹应被清洗
        insert_sent_mail(c, "demo@chinatelecom.cn", "旧名字", 100);
        insert_sent_mail(c, "demo@chinatelecom.cn", "  \"韩梅梅\"  ", 200);
    });
    assert_eq!(s.display_name(), "韩梅梅");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_name_prefers_login_account_and_rejects_foreign_senders() {
    let dir = temp_dir("display-alias");
    let s = configured_state(&dir, "");
    s.with_conn(|c| {
        // 别名地址的邮件更多，但登录账号出现过 → 只取登录账号的显示名
        for i in 0..4 {
            insert_sent_mail(c, "alias@chinatelecom.cn", "别名君", 10 + i);
        }
        insert_sent_mail(c, "demo@chinatelecom.cn", "本人", 100);
    });
    assert_eq!(s.display_name(), "本人", "登录账号出现时应优先，不取别名众数");
    let _ = std::fs::remove_dir_all(&dir);

    let dir2 = temp_dir("display-alias-only");
    let s2 = configured_state(&dir2, "");
    s2.with_conn(|c| {
        insert_sent_mail(c, "alias@chinatelecom.cn", "别名君", 100);
        insert_sent_mail(c, "alias@chinatelecom.cn", "别名君", 200);
        insert_sent_mail(c, "other@chinatelecom.cn", "别人的名字", 300);
    });
    assert_eq!(
        s2.display_name(),
        "",
        "登录账号从未出现时不得退到 From 众数（防止换账号前缀后污染）"
    );
    assert_eq!(
        load(&dir2).get(keys::DISPLAY_NAME).map(String::as_str),
        Some(""),
        "拒绝推断时不得写入任何显示名"
    );
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn display_name_skips_when_unconfigured() {
    let dir = temp_dir("display-unconf");
    let s = AppState::open(&dir);
    s.with_conn(|c| insert_sent_mail(c, "old@chinatelecom.cn", "旧账号", 100));
    assert_eq!(
        s.display_name(),
        "",
        "未配置账号时不得把历史邮件的名字写进新配置"
    );
    assert!(load(&dir).is_empty(), "不得产生任何配置写入");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- R2：迁移冻结（重启前的写入必须显式失败，不能静默落旧库） ----------

/// 迁移成功后：冻结位常驻、二次迁移被拒、收件与定时派发立刻收手
#[test]
fn move_data_dir_freezes_writes_until_restart() {
    let base = temp_dir("mig-freeze");
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    let state = AppState::open(&anchor);
    state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
    // 一封已到点的定时草稿：冻结后调度器不得把它翻成 queued
    let due = save_draft(
        &state,
        &crate::mail::DraftInput {
            id: None,
            from_addr: "demo@chinatelecom.cn".into(),
            from_name: String::new(),
            to_addrs: "peer@x.cn".into(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: "定时".into(),
            body_text: "正文".into(),
            in_reply_to: None,
            scheduled_at: now_epoch() - 5,
        },
    )
    .unwrap();

    let dest = base.join("moved");
    state
        .move_data_dir(dest.to_str().unwrap())
        .expect("首次迁移应成功");
    assert!(
        state.migration_pending(),
        "迁移成功后冻结位必须保持到进程退出"
    );
    // 二次迁移：直接拒绝（不写新快照、不动指针）
    let err = state.move_data_dir(dest.to_str().unwrap()).unwrap_err();
    assert!(err.contains("已迁移"), "二次迁移应提示重启：{err}");
    // 收件：不启动新轮，状态条给出明确原因
    state.start_fetch();
    let fs = state.fetch_snapshot();
    assert!(!fs.running, "冻结期不得启动收件轮");
    assert!(
        fs.last_error.contains("已迁移"),
        "收件应提示重启：{}",
        fs.last_error
    );
    // 定时派发：到点草稿保持 draft（不写旧库）
    assert_eq!(
        crate::mail::dispatch_due_scheduled(&state),
        0,
        "冻结期不得派发定时邮件"
    );
    let status: String = state
        .with_conn(|c| {
            c.query_row("SELECT status FROM emails WHERE id = ?1", [due], |r| {
                r.get(0)
            })
            .map_err(|e| e.to_string())
        })
        .unwrap();
    assert_eq!(status, "draft", "冻结期不得改写邮件状态");
    let _ = std::fs::remove_dir_all(&base);
}

/// 迁移前必须等在跑的收件轮收尾；超时返回 false（调用方据此放弃并解冻）
#[test]
fn wait_fetch_idle_times_out_while_round_running() {
    let dir = temp_dir("mig-wait");
    let state = AppState::open(&dir);
    let epoch = state.fetch_cas_begin().expect("空闲时应能起轮");
    // 本轮刚开始：看门狗不会判死，wait 只能靠超时退出
    assert!(!state.wait_fetch_idle(std::time::Duration::from_millis(150)));
    state.fetch_end(epoch, 0, "");
    assert!(
        state.wait_fetch_idle(std::time::Duration::from_millis(150)),
        "空闲轮应立即放行迁移"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
