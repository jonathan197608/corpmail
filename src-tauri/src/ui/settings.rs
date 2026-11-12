//! 设置页子系统：四 tab（账号/收件同步/存储管理/关于）的页面/片段渲染与表单保存
//!
//! 与 router.rs 拆分边界：SettingsCtx/CleanView/BackupDeleteView 模板上下文、
//! settings_* 系列 handler、账号凭据迁移、存储迁移/清理/备份删除交互。
//! 公共辅助（parse_form/trimmed_field/defaults 等）仍由 router.rs 提供
//!（pub(super) 再导出）；测试留在 router.rs（复用其测试基建）。

use super::router::{
    ConnTester, FrameCtx, RECV_PORT_DEFAULT, Req, Resp, SMTP_PORT_DEFAULT, domain_or, frame_of,
    human_bytes, parse_form, render_html, render_pane_with_sidebar_refresh, security_or,
    test_fragment, test_view, trimmed_field,
};
use crate::state::{self, AppState, MigrateOutcome, ResetFetchError, defaults, full_address, keys};
use serde::Serialize;
use std::collections::HashMap;

/// 附件走 inline BLOB 的上限区间（MB）：下限 1（0 等于全走独立文件，
/// 徒增文件数），上限 2048（再大单库体积失控，且远超邮件服务商单封上限）
const INLINE_ATTACH_MIN_MB: u32 = 1;
const INLINE_ATTACH_MAX_MB: u32 = 2048;

/// 设置页（四 tab 全量上下文：整页与 pane 片段共用一份）
#[derive(Serialize)]
pub(super) struct SettingsCtx {
    frame: FrameCtx,
    tab: String,
    // 账号 tab
    smtp_host: String,
    smtp_port: String,
    /// 加密方式（auto|implicit|starttls|plain；auto = 按端口推断）
    smtp_security: String,
    account_prefix: String,
    account_domain: String,
    display_name: String,
    /// 签名档（M3-2，多行纯文本）：撰写新邮件时预填正文底部
    signature: String,
    recv_host: String,
    recv_port: String,
    recv_security: String,
    degraded: bool,
    saved: bool,
    error: String,
    // 收件同步 tab
    sync_on_start: bool,
    /// 是否加载正文远程图片（隐私开关，默认关闭；关时 /proxy/image 回占位图）
    allow_remote_images: bool,
    sync_interval_min: String,
    first_fetch_days: String,
    /// 单封收件上限（MB）：超出只拉信头存占位
    max_mail_mb: String,
    last_sync_label: String,
    /// 收件同步 tab：刚触发「按当前范围重新拉取」（清游标+启动首拉）
    refetch_notice: bool,
    /// 收件同步 tab：收件进行中，重新拉取被拒绝（C5）
    refetch_busy: bool,
    // 存储管理 tab
    /// 配置的数据目录：迁移待生效时显示新目录（与向导页一致），否则当前目录
    data_dir: String,
    /// data_dir_pending 时展示：重启前实际仍在使用的旧目录
    data_dir_current: String,
    data_dir_pending: bool,
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
    /// 预估有待清理项（大附件/过期邮件/孤儿任一 > 0）→“立即清理”可点；
    /// 无可释放空间时置禁用（2026-11-03 需求）
    can_clean: bool,
    clean_view: Option<CleanView>,
    // 待生效的数据目录（M5 迁移成功后展示，重启后消失）
    pending_dir: String,
    /// 迁移已落盘、等待重启的冻结期（R2）：写入全被路由层拒，页面显示固定横幅
    migration_pending: bool,
    // 旧目录迁移备份（M5：锚点中的库/附件副本；仅存储 tab 且已迁移时展示）
    has_backup: bool,
    backup_size: String,
    backup_files: u64,
    backup_view: Option<BackupDeleteView>,
    // 模板 tab（B2）：邮件模板列表（其他 tab 为空）
    templates: Vec<crate::mail::templates::Template>,
    // 关于 tab
    app_version: String,
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
    /// 部分成功提示（步骤 1/2 已生效但孤儿回收被跳过）
    warning: String,
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
    // 存储页统计（estimate + 旧目录备份）一次取回（带缓存，避免打开存储页反复
    // 全库扫描）；其他 tab 不触发。迁移后台分支也走同一缓存避免返回时扫描
    let storage_stats = (tab == "storage").then(|| state.storage_stats());
    // M5 旧目录迁移备份：仅存储 tab（锚点附件）
    let (has_backup, backup_size, backup_files) = match storage_stats.as_ref() {
        Some((_, Some((b, n)))) => (true, human_bytes(*b), *n),
        Some((_, None)) => (false, String::new(), 0),
        None => (false, String::new(), 0),
    };
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let counts = state.folder_counts();
    // M5 清理预估：仅存储 tab（含孤儿 fs 扫描，其他 tab 免开销）
    let (est_bytes, est_detail, can_clean) = match storage_stats.as_ref() {
        Some((est, _)) => {
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
            (
                human_bytes(est.bytes),
                parts.join(" · "),
                est.detach_files > 0 || est.expire_emails > 0 || est.orphan_files > 0,
            )
        }
        None => (human_bytes(0), String::new(), false),
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
    let display = state.display_name();
    let prefix = g(keys::ACCOUNT_PREFIX);
    let domain = g(keys::ACCOUNT_DOMAIN);
    // 凭据降级态需在 prefix/domain 被 move 进字段前算（借用先于所有权转移）
    let degraded = state.secrets_degraded(&full_address(&prefix, &domain));
    SettingsCtx {
        // 侧栏设置入口高亮（2026-11-04 迁入 folder 列表）：设置页 frame
        // folder 记 "settings"，side.html 的 .settings-entry 据此点亮
        frame: frame_of(&prefix, &domain, &display, "settings", &counts),
        tab: tab.into(),
        smtp_host: g(keys::SMTP_HOST),
        smtp_port: g(keys::SMTP_PORT),
        smtp_security: g(keys::SMTP_SECURITY),
        account_prefix: prefix,
        account_domain: domain,
        display_name: display,
        signature: g(keys::SIGNATURE),
        recv_host: g(keys::RECV_HOST),
        recv_port: g(keys::RECV_PORT),
        recv_security: g(keys::RECV_SECURITY),
        degraded,
        saved,
        error: error.into(),
        sync_on_start: g(keys::SYNC_ON_START) == "true",
        allow_remote_images: g(keys::ALLOW_REMOTE_IMAGES) == "1",
        sync_interval_min: g(keys::SYNC_INTERVAL_MIN),
        first_fetch_days: g(keys::FIRST_FETCH_DAYS),
        max_mail_mb: g(keys::MAX_MAIL_MB),
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
        data_dir: {
            // 目录显示同步（用户反馈：向导设置的目录与设置页不一致）：
            // 迁移待生效时显示目标目录（用户在向导里选的那个），否则当前目录
            let pending = state.pending_data_dir().unwrap_or_default();
            if pending.is_empty() {
                state.data_dir().display().to_string()
            } else {
                pending
            }
        },
        data_dir_current: state.data_dir().display().to_string(),
        data_dir_pending: state.pending_data_dir().is_some(),
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
        can_clean,
        clean_view: clean,
        pending_dir: if tab == "storage" {
            state.pending_data_dir().unwrap_or_default()
        } else {
            String::new()
        },
        migration_pending: state.migration_pending(),
        has_backup,
        backup_size,
        backup_files,
        backup_view: backup,
        templates: if tab == "templates" {
            crate::mail::templates::list(state)
        } else {
            Vec::new()
        },
        app_version: env!("CARGO_PKG_VERSION").into(),
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
    // 账号页仅密码/显示名可改：账号字段为禁用态（不随表单提交），
    // 缺失时回落到已存配置——测试连接的意义是校验"新密码 + 现有服务器"
    let (prefix, domain) = match trimmed_field(&form, "account_prefix") {
        Some(p) => (p, domain_or(&form, state::ACCOUNT_DOMAIN_DEFAULT)),
        None => {
            let d = defaults();
            let p = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
            if p.is_empty() {
                return test_fragment(Vec::new(), "尚未配置账号，请先完成首次配置向导", false);
            }
            (
                p,
                state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default(),
            )
        }
    };
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
    let (label, host, port, mode) = if imap_channel {
        let host = host_state_or(&form, state, keys::RECV_HOST, state::RECV_HOST_DEFAULT);
        let port = port_state_or(&form, state, keys::RECV_PORT, RECV_PORT_DEFAULT);
        let security = security_or(&form, state, keys::RECV_SECURITY);
        (
            "收信通道",
            host,
            port,
            crate::net::TlsMode::resolve(port, &security),
        )
    } else {
        let host = host_state_or(&form, state, keys::SMTP_HOST, state::SMTP_HOST_DEFAULT);
        let port = port_state_or(&form, state, keys::SMTP_PORT, SMTP_PORT_DEFAULT);
        let security = security_or(&form, state, keys::SMTP_SECURITY);
        (
            "发信通道",
            host,
            port,
            crate::net::TlsMode::resolve(port, &security),
        )
    };
    let outcome = if imap_channel {
        tester.imap(&host, port, mode, &account, &password)
    } else {
        tester.smtp(&host, port, mode, &account, &password)
    };
    test_fragment(vec![test_view(label, &outcome)], "", true)
}

/// 账号配置键值对装配（向导"测试并保存"与设置·账号 tab 保存共用）：
/// 账号三要素 + 双通道主机/端口/加密方式。端口与方式传字符串（表单/已存值统一形态）
#[allow(clippy::too_many_arguments)]
pub(super) fn account_pairs<'a>(
    prefix: &'a str,
    domain: &'a str,
    display: &'a str,
    smtp_host: &'a str,
    smtp_port: &'a str,
    smtp_security: &'a str,
    recv_host: &'a str,
    recv_port: &'a str,
    recv_security: &'a str,
) -> [(&'static str, &'a str); 9] {
    [
        (keys::ACCOUNT_PREFIX, prefix),
        (keys::ACCOUNT_DOMAIN, domain),
        (keys::DISPLAY_NAME, display),
        (keys::SMTP_HOST, smtp_host),
        (keys::SMTP_PORT, smtp_port),
        (keys::SMTP_SECURITY, smtp_security),
        (keys::RECV_HOST, recv_host),
        (keys::RECV_PORT, recv_port),
        (keys::RECV_SECURITY, recv_security),
    ]
}

/// 设置·账号 tab：保存。**仅允许修改密码与发件显示名**——账号/域名/双通道
/// 服务器与加密方式由首次配置确定：一律取已存值，表单同名字段被忽略
/// （页面渲染为禁用态不提交；服务端再拦一道，防绕过 UI 的提交改掉配置）。
/// 密码留空 = 不变。
pub(super) fn settings_account_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let d = defaults();
    let prefix = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
    if prefix.is_empty() {
        return account_pane(state, false, "尚未配置账号，请先完成首次配置向导");
    }
    let account = full_address(
        &prefix,
        &state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default(),
    );
    // 不可改字段：全部取已存值（含 defaults 兜底），忽略任何表单提交
    let smtp_host = state
        .get_or(keys::SMTP_HOST, &d)
        .unwrap_or_else(|| state::SMTP_HOST_DEFAULT.to_string());
    let recv_host = state
        .get_or(keys::RECV_HOST, &d)
        .unwrap_or_else(|| state::RECV_HOST_DEFAULT.to_string());
    let smtp_port_s = state
        .get_or(keys::SMTP_PORT, &d)
        .unwrap_or_else(|| SMTP_PORT_DEFAULT.to_string());
    let recv_port_s = state
        .get_or(keys::RECV_PORT, &d)
        .unwrap_or_else(|| RECV_PORT_DEFAULT.to_string());
    let smtp_security = state
        .get_or(keys::SMTP_SECURITY, &d)
        .unwrap_or_else(|| state::SECURITY_DEFAULT.to_string());
    let recv_security = state
        .get_or(keys::RECV_SECURITY, &d)
        .unwrap_or_else(|| state::SECURITY_DEFAULT.to_string());
    // 可编辑字段：发件显示名 + 签名档 + 密码（留空 = 不变）
    let display = trimmed_field(&form, "display_name").unwrap_or_default();
    // 签名档：多行纯文本，不裁首尾以外的内容（replace CRLF 保持跨平台一致）
    let signature = form
        .get("signature")
        .map(|v| v.replace("\r\n", "\n"))
        .unwrap_or_default();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let mut pairs: Vec<(&str, &str)> = account_pairs(
        &prefix,
        &domain,
        &display,
        &smtp_host,
        &smtp_port_s,
        &smtp_security,
        &recv_host,
        &recv_port_s,
        &recv_security,
    )
    .to_vec();
    // 签名档随账号 tab 一并保存（多行文本，空 = 不启用）
    pairs.push((keys::SIGNATURE, signature.as_str()));
    // 用户主动清空显示名 → 写标记阻止自动推断回填；重新填写 → 清标记。
    // 自动推断本身不写此标记（审计 C2：避免错误结果被永久固化）
    pairs.push((
        keys::DISPLAY_NAME_AUTO,
        if display.is_empty() { "1" } else { "" },
    ));
    // 1) 配置先落盘：写失败即返回，凭据保持原样（旧密码不会因配置失败而丢失）
    if let Err(e) = state.set_batch(&pairs) {
        return account_pane(state, false, &format!("保存失败：{e}"));
    }
    // 2) 配置已生效再写凭据（失败时配置不回滚，但已存密码不会被清掉）
    if let Some(p) = trimmed_field(&form, "password")
        && let Err(e) = state.store_password(&account, &p)
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
    // 远程图片开关（审计 R4/R36）：缺省字段保持现值，避免旧表单提交把它关掉
    let allow_images = match form.get("allow_remote_images") {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        None => cur(keys::ALLOW_REMOTE_IMAGES) == "1",
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
    // 单封收件上限：空/非法保持现值；合法值钳制 1..=200 MB
    // （0 会让所有邮件都成占位，巨值则等于关掉保护——收件峰值内存不可控）
    let max_mail = match form.get("max_mail_mb").map(|v| v.trim().parse::<u32>()) {
        Some(Ok(v)) => v
            .clamp(state::MAX_MAIL_MB_MIN, state::MAX_MAIL_MB_MAX)
            .to_string(),
        _ => cur(keys::MAX_MAIL_MB),
    };
    // 范围变化检测（2026-11-03 Bug 修复）：扩大/缩小首拉范围后，收件箱增量
    // 游标仍指向旧范围的最大 UID——resolve_uids 兜底分支 filter(u > last.uid)
    // 会把新范围内低 UID 的历史邮件全部滤掉，表现为"改了范围收件箱不更新、
    // 手动收取提示没有新邮件"。变化即清双箱游标（走首拉分支全量重拉，入库
    // 按 remote_uid 去重，已有邮件不会重复），并立即触发收取。
    let days_changed = days != cur(keys::FIRST_FETCH_DAYS);
    if days_changed {
        // R14：范围变更（清双箱游标 + 按新范围重拉）与后台收件轮必须互斥——
        // 「判空闲 → 清游标 → 起新轮」在 reset_and_fetch 里持 fetch_state 锁
        // 原子完成。旧 check-then-act 会被恰好起跑的调度轮反写游标，范围
        // 改动静默失效。其余参数与游标同批落库：失败整体回滚（R10 两通道互回滚）
        let pairs = vec![
            (keys::SYNC_ON_START, if on { "true" } else { "false" }),
            (
                keys::ALLOW_REMOTE_IMAGES,
                if allow_images { "1" } else { "0" },
            ),
            (keys::SYNC_INTERVAL_MIN, interval.as_str()),
            (keys::FIRST_FETCH_DAYS, days.as_str()),
            (keys::MAX_MAIL_MB, max_mail.as_str()),
            (keys::LAST_UIDVALIDITY, "0"),
            (keys::LAST_UID, "0"),
            (keys::LAST_UIDVALIDITY_SENT, "0"),
            (keys::LAST_UID_SENT, "0"),
            // 首拉阶段化标志一并清除：范围重拉重新进入「先发件箱后收件箱」串行模式
            (keys::FIRST_PULL_DONE_SENT, "0"),
            (keys::FIRST_PULL_DONE_INBOX, "0"),
        ];
        return match state.reset_and_fetch(&pairs) {
            Err(ResetFetchError::Busy) => {
                // 运行轮收尾会把游标写回，此刻清了也会被覆盖：拒绝保存本轮并
                // 提示等待（其余参数随本次一并保存会误导"范围已生效"）
                let mut ctx = settings_ctx(state, "sync", false, "", None, None);
                ctx.refetch_busy = true;
                render_html(&pane_template("sync"), &ctx)
            }
            Err(ResetFetchError::Other(e)) => {
                eprintln!("[settings] 范围变更保存失败：{e}");
                sync_pane(state, false)
            }
            Ok(()) => sync_pane(state, true),
        };
    }
    let pairs = vec![
        (keys::SYNC_ON_START, if on { "true" } else { "false" }),
        (
            keys::ALLOW_REMOTE_IMAGES,
            if allow_images { "1" } else { "0" },
        ),
        (keys::SYNC_INTERVAL_MIN, interval.as_str()),
        (keys::FIRST_FETCH_DAYS, days.as_str()),
        (keys::MAX_MAIL_MB, max_mail.as_str()),
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
    let form = parse_form(&req.body);
    let d = defaults();
    let days = match form.get("first_fetch_days") {
        Some(v) if matches!(v.as_str(), "7" | "30" | "90" | "0") => v.clone(),
        _ => state.get_or(keys::FIRST_FETCH_DAYS, &d).unwrap_or_default(),
    };
    let pairs = [
        (keys::FIRST_FETCH_DAYS, days.as_str()),
        // 清空游标 → 走首拉分支（无游标 = 首次拉取），按新范围 UID SEARCH ALL / SINCE。
        // 双箱都清（2026-11-03 修复：此前漏清发件箱游标，重拉对发件箱不完整）；
        // 首拉阶段化标志一并清，重新进入「先发件箱后收件箱」串行模式
        (keys::LAST_UID, "0"),
        (keys::LAST_UIDVALIDITY, "0"),
        (keys::LAST_UID_SENT, "0"),
        (keys::LAST_UIDVALIDITY_SENT, "0"),
        (keys::FIRST_PULL_DONE_SENT, "0"),
        (keys::FIRST_PULL_DONE_INBOX, "0"),
    ];
    // C5 + R14：「判空闲 → 清游标 → 起新轮」原子完成（reset_and_fetch 持
    // fetch_state 锁）。收件进行中返回 Busy：运行轮每批会把游标写回库，
    // 不拒绝的话用户点「重新拉取」实际既清不掉游标也不拉，重拉意图丢失
    // 且无任何反馈——等待当前收取完成后重试即可
    match state.reset_and_fetch(&pairs) {
        Err(ResetFetchError::Busy) => {
            let mut ctx = settings_ctx(state, "sync", false, "", None, None);
            ctx.refetch_busy = true;
            render_html(&pane_template("sync"), &ctx)
        }
        Err(ResetFetchError::Other(e)) => {
            eprintln!("[settings] 重新拉取启动失败：{e}");
            sync_pane(state, false)
        }
        Ok(()) => {
            let mut ctx = settings_ctx(state, "sync", false, "", None, None);
            ctx.refetch_notice = true;
            render_html(&pane_template("sync"), &ctx)
        }
    }
}

/// 设置·存储管理 tab：保存（清理参数在 M5 清理器实施时生效）
pub(super) fn settings_storage_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let d = defaults();
    let cur = |k: &str| state.get_or(k, &d).unwrap_or_default();

    // 必填数字：空/非法保持现值；合法值钳制到 1..=2048 MB
    // （旧实现只校验"是数字"，填 40 亿会让所有附件都走 inline BLOB，撑爆单库）
    let inline = match form
        .get("inline_attach_max_mb")
        .map(|v| v.trim().parse::<u32>())
    {
        Some(Ok(v)) => v
            .clamp(INLINE_ATTACH_MIN_MB, INLINE_ATTACH_MAX_MB)
            .to_string(),
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
    // 新策略落地即同步执行一轮清理（个人邮箱规模毫秒级）：改短保留时长
    // 立即可见效果，无需等每日定时器或手动点「立即清理」（2026-11-03 需求：
    // 此前只改值不清理，“收件保留时长”看起来不生效）。清理失败不影响保存成功
    let view = clean_view_of(crate::clean::run(state));
    // 清理改了库 → 存储统计缓存失效，下次打开重新计算
    state.invalidate_stats_cache();
    // 清理可能删邮件 → 边栏计数需要同步刷新（HX-Trigger 事件驱动局部刷新）
    render_pane_with_sidebar_refresh(
        &pane_template("storage"),
        &settings_ctx(state, "storage", true, "", Some(view), None),
    )
}

/// 设置·存储管理：迁移数据目录（M5；dest 由前端系统对话框选定后 POST）。
/// 在线快照（VACUUM INTO）+ 指针写入锚点库，重启后生效；回 pane 片段：
/// 成功时展示"待生效目录"行，失败时 error 区展示原因（对齐立即清理的交互模式）
pub(super) fn settings_storage_move(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let dest = form.get("dest").cloned().unwrap_or_default();
    match state.start_migration(&dest) {
        // 后台迁移已启动（生产）：协议回调立即返回轻量片段，前端据此开阻塞模态，
        // 进度/结果经事件回传（不再在协议回调内同步执行 VACUUM/附件复制）
        Ok(MigrateOutcome::Background) => Resp::html(
            "<div class=\"settings-pane\" id=\"settings-pane\"><p class=\"desc\">数据迁移正在进行中，请稍候…</p></div>".into(),
        ),
        // 同步完成（单元测试 / 未注入 executor）：保持既有 pane 返回语义
        Ok(MigrateOutcome::Done) => render_html(
            &pane_template("storage"),
            &settings_ctx(state, "storage", false, "", None, None),
        ),
        Err(e) => render_html(
            &pane_template("storage"),
            &settings_ctx(state, "storage", false, &e, None, None),
        ),
    }
}

/// 清理结果 → 展示视图（手动/保存后自动共用）
fn clean_view_of(result: Result<crate::clean::CleanReport, String>) -> CleanView {
    match result {
        Ok(report) => CleanView {
            ok: true,
            freed: human_bytes(report.freed_bytes),
            detached: report.detached_attachments,
            removed: report.removed_emails,
            orphans: report.orphan_files,
            error: String::new(),
            warning: report.warning,
        },
        Err(err) => CleanView {
            ok: false,
            freed: String::new(),
            detached: 0,
            removed: 0,
            orphans: 0,
            error: err,
            warning: String::new(),
        },
    }
}

/// 设置·存储管理：立即清理（手动触发，同步执行；回 pane 带报告）
pub(super) fn settings_storage_clean(state: &AppState) -> Resp {
    let view = clean_view_of(crate::clean::run(state));
    // 清理改了库 → 存储统计缓存失效，下次打开重新计算
    state.invalidate_stats_cache();
    // 清理删行后边栏计数需刷新（同保存路径）
    render_pane_with_sidebar_refresh(
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
