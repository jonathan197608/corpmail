//! state 模块单元测试（自 src/state.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;
use crate::conf::{load, save};
use crate::db::{open, settings};
use crate::mail::{now_epoch, save_draft};

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
