//! 设置页子系统：四 tab（账号/收件同步/存储管理/关于）的页面/片段渲染与表单保存
//!
//! 与 router.rs 拆分边界：SettingsCtx/CleanView/BackupDeleteView 模板上下文、
//! settings_* 系列 handler、账号凭据迁移、存储迁移/清理/备份删除交互。
//! 公共辅助（parse_form/trimmed_field/defaults 等）仍由 router.rs 提供
//!（pub(super) 再导出）；测试留在 router.rs（复用其测试基建）。

use super::router::{
    ConnTester, FrameCtx, RECV_PORT_DEFAULT, Req, Resp, SMTP_PORT_DEFAULT, domain_or, frame_of,
    human_bytes, parse_form, render_html, test_fragment, test_view, trimmed_field,
};
use crate::state::{self, AppState, defaults, full_address, keys};
use serde::Serialize;
use std::collections::HashMap;

/// 设置页（四 tab 全量上下文：整页与 pane 片段共用一份）
#[derive(Serialize)]
pub(super) struct SettingsCtx {
    frame: FrameCtx,
    tab: String,
    // 账号 tab
    smtp_host: String,
    smtp_port: String,
    account_prefix: String,
    account_domain: String,
    display_name: String,
    recv_host: String,
    recv_port: String,
    degraded: bool,
    saved: bool,
    error: String,
    // 收件同步 tab
    sync_on_start: bool,
    sync_interval_min: String,
    first_fetch_days: String,
    last_sync_label: String,
    /// 收件同步 tab：刚触发「按当前范围重新拉取」（清游标+启动首拉）
    refetch_notice: bool,
    /// 收件同步 tab：收件进行中，重新拉取被拒绝（C5）
    refetch_busy: bool,
    // 存储管理 tab
    data_dir: String,
    inline_max: String,
    db_size: String,
    db_pct: u32,
    attach_size: String,
    attach_files: u64,
    attach_pct: u32,
    clean_days: String,
    keep_inbox: String,
    keep_sent: String,
    // 存储管理 tab（M5 清理器）：预估 + 手动清理报告
    clean_estimate_bytes: String,
    clean_estimate_detail: String,
    clean_view: Option<CleanView>,
    // 待生效的数据目录（M5 迁移成功后展示，重启后消失）
    pending_dir: String,
    // 旧目录迁移备份（M5：锚点中的库/附件副本；仅存储 tab 且已迁移时展示）
    has_backup: bool,
    backup_size: String,
    backup_files: u64,
    backup_view: Option<BackupDeleteView>,
    // 关于 tab
    app_version: String,
    sqlite_version: String,
}

/// 手动清理结果展示（存储页报告区；ok=false 时仅展示 error）
#[derive(Serialize)]
pub(super) struct CleanView {
    ok: bool,
    freed: String,
    detached: u64,
    removed: u64,
    orphans: u64,
    error: String,
}

/// 删除迁移备份结果展示（存储页；ok=false 时展示失败原因）
#[derive(Serialize)]
pub(super) struct BackupDeleteView {
    ok: bool,
    msg: String,
}

pub(super) fn settings_page(state: &AppState, tab: &str) -> Resp {
    render_html(
        "settings.html",
        &settings_ctx(state, tab, false, "", None, None),
    )
}

pub(super) fn settings_pane_fragment(state: &AppState, tab: &str) -> Resp {
    render_html(
        &pane_template(tab),
        &settings_ctx(state, tab, false, "", None, None),
    )
}

pub(super) fn pane_template(tab: &str) -> String {
    format!("settings_pane_{tab}.html")
}

pub(super) fn settings_ctx(
    state: &AppState,
    tab: &str,
    saved: bool,
    error: &str,
    clean: Option<CleanView>,
    backup: Option<BackupDeleteView>,
) -> SettingsCtx {
    // M5 旧目录迁移备份：仅存储 tab 计算（递归扫描锚点附件）
    let (has_backup, backup_size, backup_files) = if tab == "storage" {
        match state.old_backup_stats() {
            Some((b, n)) => (true, human_bytes(b), n),
            None => (false, String::new(), 0),
        }
    } else {
        (false, String::new(), 0)
    };
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let counts = state.folder_counts();
    // M5 清理预估：仅存储 tab 计算（含孤儿 fs 扫描，其他 tab 免开销）
    let (est_bytes, est_detail) = if tab == "storage" {
        let est = crate::clean::estimate(state);
        let mut parts: Vec<String> = Vec::new();
        if est.detach_files > 0 {
            parts.push(format!("大附件 {} 个", est.detach_files));
        }
        if est.expire_emails > 0 {
            parts.push(format!("过期邮件 {} 封", est.expire_emails));
        }
        if est.orphan_files > 0 {
            parts.push(format!("孤儿文件 {} 个", est.orphan_files));
        }
        (human_bytes(est.bytes), parts.join(" · "))
    } else {
        (human_bytes(0), String::new())
    };
    let (attach_bytes, attach_files) = state.attachments_stats();
    let db_bytes = state.db_size_bytes();
    let total = db_bytes + attach_bytes;
    // 空库时两段占比均为 0（不能把 100% 错记到附件段）
    let (db_pct, attach_pct) = match db_bytes.saturating_mul(100).checked_div(total) {
        Some(p) => {
            let p = p.min(100) as u32;
            (p, 100 - p)
        }
        None => (0, 0),
    };
    // 账号三要素一次读取：frame 与各 tab 字段复用（避免重复读配置/库锁）
    let display = g(keys::DISPLAY_NAME);
    let prefix = g(keys::ACCOUNT_PREFIX);
    let domain = g(keys::ACCOUNT_DOMAIN);
    // 凭据降级态需在 prefix/domain 被 move 进字段前算（借用先于所有权转移）
    let degraded = state.secrets_degraded(&full_address(&prefix, &domain));
    SettingsCtx {
        frame: frame_of(&prefix, &domain, &display, "", &counts),
        tab: tab.into(),
        smtp_host: g(keys::SMTP_HOST),
        smtp_port: g(keys::SMTP_PORT),
        account_prefix: prefix,
        account_domain: domain,
        display_name: display,
        recv_host: g(keys::RECV_HOST),
        recv_port: g(keys::RECV_PORT),
        degraded,
        saved,
        error: error.into(),
        sync_on_start: g(keys::SYNC_ON_START) == "true",
        sync_interval_min: g(keys::SYNC_INTERVAL_MIN),
        first_fetch_days: g(keys::FIRST_FETCH_DAYS),
        refetch_notice: false,
        refetch_busy: false,
        last_sync_label: {
            let at = g(keys::LAST_SYNC_AT).parse::<i64>().unwrap_or(0);
            if at > 0 {
                format!("上次收取：{}", crate::mail::time_full(at))
            } else {
                "尚未收取过邮件".into()
            }
        },
        data_dir: state.data_dir.display().to_string(),
        inline_max: g(keys::INLINE_ATTACH_MAX_MB),
        db_size: human_bytes(db_bytes),
        db_pct,
        attach_size: human_bytes(attach_bytes),
        attach_files,
        attach_pct,
        clean_days: g(keys::CLEAN_SENT_BULK_DAYS),
        keep_inbox: g(keys::KEEP_INBOX_DAYS),
        keep_sent: g(keys::KEEP_SENT_DAYS),
        // M5 清理预估与手动清理报告（仅存储 tab 有意义）
        clean_estimate_bytes: est_bytes,
        clean_estimate_detail: est_detail,
        clean_view: clean,
        pending_dir: if tab == "storage" {
            state.pending_data_dir().unwrap_or_default()
        } else {
            String::new()
        },
        has_backup,
        backup_size,
        backup_files,
        backup_view: backup,
        app_version: env!("CARGO_PKG_VERSION").into(),
        sqlite_version: state.sqlite_version(),
    }
}

pub(super) fn account_pane(state: &AppState, saved: bool, error: &str) -> Resp {
    render_html(
        &pane_template("account"),
        &settings_ctx(state, "account", saved, error, None, None),
    )
}

pub(super) fn sync_pane(state: &AppState, saved: bool) -> Resp {
    render_html(
        &pane_template("sync"),
        &settings_ctx(state, "sync", saved, "", None, None),
    )
}

pub(super) fn storage_pane(state: &AppState, saved: bool) -> Resp {
    render_html(
        &pane_template("storage"),
        &settings_ctx(state, "storage", saved, "", None, None),
    )
}

/// 设置·账号 tab：测试连接（SMTP）/测试收信（IMAP）
/// 密码取值：表单优先，留空读已保存凭据（设计 §8.2）
pub(super) fn settings_account_test(
    state: &AppState,
    req: &Req,
    tester: &dyn ConnTester,
    imap_channel: bool,
) -> Resp {
    let form = parse_form(&req.body);
    let Some(prefix) = trimmed_field(&form, "account_prefix") else {
        return test_fragment(Vec::new(), "请先填写登录账号", false);
    };
    let domain = domain_or(&form, state::ACCOUNT_DOMAIN_DEFAULT);
    let account = full_address(&prefix, &domain);
    let password = match trimmed_field(&form, "password") {
        Some(p) => p,
        None => match state.retrieve_password(&account) {
            crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => p,
            crate::secrets::GetResult::Missing => {
                return test_fragment(
                    Vec::new(),
                    "未找到已保存的密码，请先保存或在密码框输入",
                    false,
                );
            }
        },
    };
    let (label, host, port) = if imap_channel {
        let host = host_state_or(&form, state, keys::RECV_HOST, state::RECV_HOST_DEFAULT);
        let port = port_state_or(&form, state, keys::RECV_PORT, RECV_PORT_DEFAULT);
        ("收信通道", host, port)
    } else {
        let host = host_state_or(&form, state, keys::SMTP_HOST, state::SMTP_HOST_DEFAULT);
        let port = port_state_or(&form, state, keys::SMTP_PORT, SMTP_PORT_DEFAULT);
        ("发信通道", host, port)
    };
    let outcome = if imap_channel {
        tester.imap(&host, port, &account, &password)
    } else {
        tester.smtp(&host, port, &account, &password)
    };
    test_fragment(vec![test_view(label, &outcome)], "", true)
}

/// 账号配置键值对装配（向导"测试并保存"与设置·账号 tab 保存共用）：
/// 账号三要素 + 双通道主机/端口。端口传字符串（表单/已存值统一形态）
pub(super) fn account_pairs<'a>(
    prefix: &'a str,
    domain: &'a str,
    display: &'a str,
    smtp_host: &'a str,
    smtp_port: &'a str,
    recv_host: &'a str,
    recv_port: &'a str,
) -> [(&'static str, &'a str); 7] {
    [
        (keys::ACCOUNT_PREFIX, prefix),
        (keys::ACCOUNT_DOMAIN, domain),
        (keys::DISPLAY_NAME, display),
        (keys::SMTP_HOST, smtp_host),
        (keys::SMTP_PORT, smtp_port),
        (keys::RECV_HOST, recv_host),
        (keys::RECV_PORT, recv_port),
    ]
}

/// 设置·账号 tab：保存（密码留空 = 不变；账号变更自动迁移凭据）
pub(super) fn settings_account_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let Some(prefix) = trimmed_field(&form, "account_prefix") else {
        return account_pane(state, false, "登录账号不能为空");
    };
    let d = defaults();
    let old_prefix = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
    let old_domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let domain = domain_or(&form, state::ACCOUNT_DOMAIN_DEFAULT);
    let new_account = full_address(&prefix, &domain);
    let new_password = trimmed_field(&form, "password");
    let old_account = full_address(&old_prefix, &old_domain);
    let account_changed = prefix != old_prefix || domain != old_domain;

    // 1) 凭据预检：账号变更且密码框留空时，旧账号必须存在可迁移凭据。
    //    先预检再落配置——预检失败不得留下"配置已改、密码未迁"的半状态。
    let migrate_from = if account_changed && new_password.is_none() {
        match state.retrieve_password(&old_account) {
            crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => {
                Some(p)
            }
            crate::secrets::GetResult::Missing => {
                return account_pane(state, false, "账号已修改，请输入密码后保存");
            }
        }
    } else {
        None
    };

    let smtp_host = host_state_or(&form, state, keys::SMTP_HOST, state::SMTP_HOST_DEFAULT);
    let recv_host = host_state_or(&form, state, keys::RECV_HOST, state::RECV_HOST_DEFAULT);
    let smtp_port_s = port_state_or(&form, state, keys::SMTP_PORT, SMTP_PORT_DEFAULT).to_string();
    let recv_port_s = port_state_or(&form, state, keys::RECV_PORT, RECV_PORT_DEFAULT).to_string();
    let display = trimmed_field(&form, "display_name").unwrap_or_default();
    let pairs = account_pairs(
        &prefix,
        &domain,
        &display,
        &smtp_host,
        &smtp_port_s,
        &recv_host,
        &recv_port_s,
    );
    // 2) 配置先落盘：写失败即返回，凭据保持原样（旧密码不会因配置失败而丢失）
    if let Err(e) = state.set_batch(&pairs) {
        return account_pane(state, false, &format!("保存失败：{e}"));
    }

    // 3) 配置已生效，再写/迁移凭据（失败时配置不回滚，但已存密码不会被清掉）
    if account_changed {
        let target = new_password.clone().or(migrate_from);
        if let Some(p) = target
            && let Err(e) = state.store_password(&new_account, &p)
        {
            return account_pane(state, false, &format!("密码保存失败：{e}"));
        }
        if !old_prefix.is_empty() {
            state.clear_password(&old_account);
        }
    } else if let Some(p) = &new_password
        && let Err(e) = state.store_password(&new_account, p)
    {
        return account_pane(state, false, &format!("密码保存失败：{e}"));
    }
    account_pane(state, true, "")
}

/// 设置·收件同步 tab：保存（参数在 M4 收件实施时生效）
pub(super) fn settings_sync_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let d = defaults();
    let cur = |k: &str| state.get_or(k, &d).unwrap_or_default();

    let on = match form.get("sync_on_start") {
        Some(v) => v == "true",
        None => cur(keys::SYNC_ON_START) == "true",
    };
    let interval = match form.get("sync_interval_min") {
        // 上限 7 天：调度线程按间隔 sleep，巨值会让定时收件永久失效（重启才恢复）
        Some(v) => match v.trim().parse::<u32>() {
            Ok(n) => n.min(10_080).to_string(),
            Err(_) => cur(keys::SYNC_INTERVAL_MIN),
        },
        None => cur(keys::SYNC_INTERVAL_MIN),
    };
    let days = match form.get("first_fetch_days") {
        Some(v) if matches!(v.as_str(), "7" | "30" | "90" | "0") => v.clone(),
        _ => cur(keys::FIRST_FETCH_DAYS),
    };
    let pairs = [
        (keys::SYNC_ON_START, if on { "true" } else { "false" }),
        (keys::SYNC_INTERVAL_MIN, interval.as_str()),
        (keys::FIRST_FETCH_DAYS, days.as_str()),
    ];
    if state.set_batch(&pairs).is_err() {
        // 保存失败回渲染当前值，用户重试（库错误在此无更优交互）
        return sync_pane(state, false);
    }
    sync_pane(state, true)
}

/// 设置·收件同步 tab：「按当前范围重新拉取」（M6）
/// 读取选中范围 → 清空收件游标（LAST_UID/LAST_UIDVALIDITY）→ 立即触发一轮首拉。
/// 清游标让下次 fetch 走进「无游标→首拉」分支，按新范围（全部/天数）全量重拉，
/// 解决"改首拉范围后历史不补拉"的缺口。
pub(super) fn settings_sync_refetch(state: &AppState, req: &Req) -> Resp {
    // C5：收件进行中直接拒绝——运行轮每批会把游标写回库，叠加 start_fetch 防抖
    // （CAS 失败静默忽略），若不拒绝，用户点「重新拉取」实际既清不掉游标也不拉，
    // 重拉意图丢失且无任何反馈。等待当前收取完成后重试即可。
    if state.fetch_snapshot().running {
        let mut ctx = settings_ctx(state, "sync", false, "", None, None);
        ctx.refetch_busy = true;
        return render_html(&pane_template("sync"), &ctx);
    }
    let form = parse_form(&req.body);
    let d = defaults();
    let days = match form.get("first_fetch_days") {
        Some(v) if matches!(v.as_str(), "7" | "30" | "90" | "0") => v.clone(),
        _ => state.get_or(keys::FIRST_FETCH_DAYS, &d).unwrap_or_default(),
    };
    let pairs = [
        (keys::FIRST_FETCH_DAYS, days.as_str()),
        // 清空游标 → 走首拉分支（无游标 = 首次拉取），按新范围 UID SEARCH ALL / SINCE
        (keys::LAST_UID, "0"),
        (keys::LAST_UIDVALIDITY, "0"),
    ];
    if state.set_batch(&pairs).is_err() {
        return sync_pane(state, false);
    }
    state.start_fetch(); // 后台立即拉取；已在收件中则防抖忽略
    let mut ctx = settings_ctx(state, "sync", false, "", None, None);
    ctx.refetch_notice = true;
    render_html(&pane_template("sync"), &ctx)
}

/// 设置·存储管理 tab：保存（清理参数在 M5 清理器实施时生效）
pub(super) fn settings_storage_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let d = defaults();
    let cur = |k: &str| state.get_or(k, &d).unwrap_or_default();

    // 必填数字：空/非法保持现值
    let inline = match form.get("inline_attach_max_mb") {
        Some(v) if v.trim().parse::<u32>().is_ok() => v.trim().to_string(),
        _ => cur(keys::INLINE_ATTACH_MAX_MB),
    };
    // 数字或空：空 = 永久保留（设计 §5.2）；非法保持现值
    let clean = num_or_blank(
        &form,
        "clean_sent_bulk_days",
        &cur(keys::CLEAN_SENT_BULK_DAYS),
    );
    let keep_inbox = num_or_blank(&form, "keep_inbox_days", &cur(keys::KEEP_INBOX_DAYS));
    let keep_sent = num_or_blank(&form, "keep_sent_days", &cur(keys::KEEP_SENT_DAYS));

    let pairs = [
        (keys::INLINE_ATTACH_MAX_MB, inline.as_str()),
        (keys::CLEAN_SENT_BULK_DAYS, clean.as_str()),
        (keys::KEEP_INBOX_DAYS, keep_inbox.as_str()),
        (keys::KEEP_SENT_DAYS, keep_sent.as_str()),
    ];
    if state.set_batch(&pairs).is_err() {
        // 保存失败也回渲染当前值，用户重试（库错误在此无更优交互）
        return storage_pane(state, false);
    }
    storage_pane(state, true)
}

/// 设置·存储管理：迁移数据目录（M5；dest 由前端系统对话框选定后 POST）。
/// 在线快照（VACUUM INTO）+ 指针写入锚点库，重启后生效；回 pane 片段：
/// 成功时展示"待生效目录"行，失败时 error 区展示原因（对齐立即清理的交互模式）
pub(super) fn settings_storage_move(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let dest = form.get("dest").cloned().unwrap_or_default();
    let error = if dest.trim().is_empty() {
        "未选择目标目录".to_string()
    } else {
        match state.move_data_dir(&dest) {
            Ok(_) => String::new(),
            Err(e) => e,
        }
    };
    render_html(
        &pane_template("storage"),
        &settings_ctx(state, "storage", false, &error, None, None),
    )
}

/// 设置·存储管理：立即清理（手动触发，同步执行；回 pane 带报告）
pub(super) fn settings_storage_clean(state: &AppState) -> Resp {
    let view = match crate::clean::run(state) {
        Ok(report) => CleanView {
            ok: true,
            freed: human_bytes(report.freed_bytes),
            detached: report.detached_attachments,
            removed: report.removed_emails,
            orphans: report.orphan_files,
            error: String::new(),
        },
        Err(err) => CleanView {
            ok: false,
            freed: String::new(),
            detached: 0,
            removed: 0,
            orphans: 0,
            error: err,
        },
    };
    render_html(
        &pane_template("storage"),
        &settings_ctx(state, "storage", false, "", Some(view), None),
    )
}

/// 设置·存储管理：删除旧目录迁移备份（M5 验收功能；清锚点 corpmail.db[-wal/-shm]
/// 与 attachments/，配置文件 corpmail.toml 与当前数据不受影响；回 pane 带结果）
pub(super) fn settings_backup_delete(state: &AppState) -> Resp {
    let view = match state.delete_old_backup() {
        Ok((freed, files)) => BackupDeleteView {
            ok: true,
            msg: format!(
                "已删除迁移备份，释放 {}（{} 个文件）",
                human_bytes(freed),
                files
            ),
        },
        Err(err) => BackupDeleteView {
            ok: false,
            msg: err,
        },
    };
    render_html(
        &pane_template("storage"),
        &settings_ctx(state, "storage", false, "", None, Some(view)),
    )
}

/// 主机字段：表单值优先，空则取已保存值（设置页语义；表单字段名与 settings 键同名）
fn host_state_or(
    form: &HashMap<String, String>,
    state: &AppState,
    key: &str,
    fallback: &str,
) -> String {
    let d = defaults();
    let current = state.get_or(key, &d).unwrap_or_default();
    match trimmed_field(form, key) {
        Some(v) => v,
        None => {
            if current.is_empty() {
                fallback.to_string()
            } else {
                current
            }
        }
    }
}

/// 端口字段：表单值优先，空/非法取已保存值，再退 fallback（设置页语义）
fn port_state_or(
    form: &HashMap<String, String>,
    state: &AppState,
    key: &str,
    fallback: u16,
) -> u16 {
    let d = defaults();
    let current = state.get_or(key, &d).unwrap_or_default();
    form.get(key)
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| current.trim().parse().ok())
        .unwrap_or(fallback)
}

/// 数字或空字段：空 = 永久保留（合法值）；非法 = 保持现值
fn num_or_blank(form: &HashMap<String, String>, name: &str, current: &str) -> String {
    match form.get(name).map(|s| s.trim()) {
        Some("") => String::new(),
        Some(s) if s.parse::<u32>().is_ok() => s.to_string(),
        _ => current.to_string(),
    }
}
