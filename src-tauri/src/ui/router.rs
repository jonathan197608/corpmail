//! 路由：把 `ui://` 请求映射为 HTML 页面/片段或静态资源响应（M1 版）
//!
//! - 非向导不可达原则：未完成首启配置时根路径渲染欢迎页
//! - 网络测试（SMTP/IMAP）经 `ConnTester` 注入：生产走 `conncheck` 真连，
//!   单测注入假实现（协议回调同步语义，测试期间阻塞可接受）
//! - htmx 片段响应：pane 片段（tab 切换/保存回显）与 test_result 片段（连接测试）
//! - 向导保存成功以 `HX-Redirect` 响应头驱动整页跳转（htmx 约定）

use crate::conncheck::TestOutcome;
use crate::state::{self, AppState, defaults, full_address, keys};
use crate::templates;
use crate::ui::Assets;
use serde::Serialize;
use std::collections::HashMap;
use url::form_urlencoded;

/// 连接测试注入点（生产实现真连服务器；单测注入假实现避免网络依赖）
pub trait ConnTester {
    fn smtp(&self, host: &str, port: u16, account: &str, password: &str) -> TestOutcome;
    fn imap(&self, host: &str, port: u16, account: &str, password: &str) -> TestOutcome;
}

/// 生产实现：直连真实服务器（设计 §8.1 双通道测试）
pub struct RealTester;

impl ConnTester for RealTester {
    fn smtp(&self, host: &str, port: u16, account: &str, password: &str) -> TestOutcome {
        crate::conncheck::smtp_test(host, port, account, password)
    }

    fn imap(&self, host: &str, port: u16, account: &str, password: &str) -> TestOutcome {
        crate::conncheck::imap_test(host, port, account, password)
    }
}

/// 平台无关请求（协议层参数脱壳后的形态）
pub struct Req {
    pub method: String,
    /// 以 `/` 开头、不含查询串
    pub path: String,
    /// 是否带 `HX-Request` 头（htmx 发起的片段请求）。
    /// M1 仅 debug 日志读取（release 下无读者）；M2 起 `/` 等整页路由将按它区分片段响应
    #[allow(dead_code)]
    pub is_htmx_request: bool,
    pub body: Vec<u8>,
}

impl Req {
    /// 从协议层请求脱壳（测试直接构造本结构体，不走 tauri::http）
    pub fn parse_from(request: &tauri::http::Request<Vec<u8>>) -> Self {
        // HeaderName 统一小写，get 按大小写不敏感匹配
        Self {
            method: request.method().as_str().to_string(),
            path: request.uri().path().to_string(),
            is_htmx_request: request.headers().get("hx-request").is_some(),
            body: request.body().clone(),
        }
    }
}

/// 平台无关响应
pub struct Resp {
    pub status: u16,
    pub content_type: String,
    pub cache_control: &'static str,
    pub body: Vec<u8>,
    /// 附加响应头（HX-Redirect 等）
    pub extra_headers: Vec<(String, String)>,
}

impl Resp {
    fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            cache_control: "no-store",
            body: body.into_bytes(),
            extra_headers: Vec::new(),
        }
    }

    /// htmx 整页跳转（htmx 读取 HX-Redirect 响应头后驱动 window.location）
    fn hx_redirect(url: &str) -> Self {
        let mut resp = Self::html(String::new());
        resp.extra_headers.push(("HX-Redirect".into(), url.into()));
        resp
    }

    fn asset(mime: String, body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: mime,
            cache_control: "max-age=86400",
            body,
            extra_headers: Vec::new(),
        }
    }

    fn not_found() -> Self {
        Self::html(error_page(
            "404 · 页面不存在",
            "请求的路径没有对应的资源（静态文件或动态路由均未命中）。",
        ))
        .with_status(404)
    }

    fn internal_error(message: String) -> Self {
        Self::html(error_page("500 · 渲染失败", &message)).with_status(500)
    }

    fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

// ---------- 模板上下文 ----------

/// 侧栏（主界面/设置页共用）
#[derive(Serialize)]
struct FrameCtx {
    display_name: String,
    full_addr: String,
    avatar: String,
    inbox: i64,
    inbox_unread: i64,
    all_mail: i64,
    sent: i64,
    draft: i64,
}

#[derive(Serialize)]
struct MainCtx {
    frame: FrameCtx,
}

/// 首启向导·账号配置页（未配置字段为空，预填服务器默认值）
#[derive(Serialize)]
struct SetupCtx {
    account_prefix: String,
    account_domain: String,
    display_name: String,
    smtp_host: String,
    smtp_port: String,
    recv_host: String,
    recv_port: String,
}

/// 首启向导·完成页
#[derive(Serialize)]
struct DoneCtx {
    account: String,
    display_name: String,
    smtp_host: String,
    smtp_port: String,
    recv_host: String,
    recv_port: String,
}

/// 设置页（四 tab 全量上下文：整页与 pane 片段共用一份）
#[derive(Serialize)]
struct SettingsCtx {
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
    // 关于 tab
    app_version: String,
    sqlite_version: String,
}

/// 连接测试结果片段
#[derive(Serialize)]
struct TestResultCtx {
    tests: Vec<TestView>,
    note: String,
    note_ok: bool,
}

#[derive(Serialize)]
struct TestView {
    label: String,
    ok: bool,
    elapsed_ms: u64,
    detail: String,
}

fn test_view(label: &str, o: &TestOutcome) -> TestView {
    TestView {
        label: label.into(),
        ok: o.ok,
        elapsed_ms: o.elapsed_ms,
        detail: o.detail.clone(),
    }
}

// ---------- 总路由 ----------

/// 总路由：动态路由优先，静态资源兜底，最后 404
pub fn route<A: Assets>(state: &AppState, assets: &A, req: &Req, tester: &dyn ConnTester) -> Resp {
    if let Some(resp) = dynamic(state, req, tester) {
        return resp;
    }
    serve_asset(assets, req).unwrap_or_else(Resp::not_found)
}

/// 动态路由（页面与片段），未命中返回 None 交给静态资源
fn dynamic(state: &AppState, req: &Req, tester: &dyn ConnTester) -> Option<Resp> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => Some(root(state)),
        ("GET", "/welcome") => Some(render_html("welcome.html", &())),
        ("GET", "/welcome/setup") => Some(wizard_setup_page(state)),
        ("POST", "/welcome/setup/save") => Some(wizard_save(state, req, tester)),
        ("GET", "/welcome/done") => Some(wizard_done(state)),
        ("GET", "/settings") => Some(settings_page(state, "account")),
        ("GET", "/settings/account")
        | ("GET", "/settings/sync")
        | ("GET", "/settings/storage")
        | ("GET", "/settings/about") => {
            let tab = req.path.rsplit('/').next().unwrap_or("account");
            Some(settings_page(state, tab))
        }
        ("GET", p) if p.starts_with("/settings/panes/") => {
            let tab = p.strip_prefix("/settings/panes/").unwrap_or("");
            valid_tab(tab).then(|| settings_pane_fragment(state, tab))
        }
        ("POST", "/settings/account/save") => Some(settings_account_save(state, req)),
        ("POST", "/settings/account/test-smtp") => {
            Some(settings_account_test(state, req, tester, false))
        }
        ("POST", "/settings/account/test-imap") => {
            Some(settings_account_test(state, req, tester, true))
        }
        ("POST", "/settings/sync/save") => Some(settings_sync_save(state, req)),
        ("POST", "/settings/storage/save") => Some(settings_storage_save(state, req)),
        _ => None,
    }
}

fn valid_tab(tab: &str) -> bool {
    matches!(tab, "account" | "sync" | "storage" | "about")
}

// ---------- 页面 ----------

fn root(state: &AppState) -> Resp {
    if state.configured() {
        render_html(
            "main.html",
            &MainCtx {
                frame: frame_ctx(state),
            },
        )
    } else {
        // 首启未配置：配置为强制入口（设计 §8.1，无"稍后再说"）
        render_html("welcome.html", &())
    }
}

fn wizard_setup_page(state: &AppState) -> Resp {
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let ctx = SetupCtx {
        account_prefix: g(keys::ACCOUNT_PREFIX),
        account_domain: g(keys::ACCOUNT_DOMAIN),
        display_name: g(keys::DISPLAY_NAME),
        smtp_host: g(keys::SMTP_HOST),
        smtp_port: g(keys::SMTP_PORT),
        recv_host: g(keys::RECV_HOST),
        recv_port: g(keys::RECV_PORT),
    };
    render_html("setup.html", &ctx)
}

fn wizard_done(state: &AppState) -> Resp {
    if !state.configured() {
        // 未配置直接访问完成页：回欢迎页（无重定向，协议层保持简单）
        return render_html("welcome.html", &());
    }
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let ctx = DoneCtx {
        account: full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN)),
        display_name: g(keys::DISPLAY_NAME),
        smtp_host: g(keys::SMTP_HOST),
        smtp_port: g(keys::SMTP_PORT),
        recv_host: g(keys::RECV_HOST),
        recv_port: g(keys::RECV_PORT),
    };
    render_html("done.html", &ctx)
}

fn settings_page(state: &AppState, tab: &str) -> Resp {
    render_html("settings.html", &settings_ctx(state, tab, false, ""))
}

fn settings_pane_fragment(state: &AppState, tab: &str) -> Resp {
    render_html(&pane_template(tab), &settings_ctx(state, tab, false, ""))
}

fn pane_template(tab: &str) -> String {
    format!("settings_pane_{tab}.html")
}

// ---------- 表单与保存 ----------

/// 向导"测试并保存"：双通道全部通过才落库（设计 §8.1）
fn wizard_save(state: &AppState, req: &Req, tester: &dyn ConnTester) -> Resp {
    let form = parse_form(&req.body);
    let prefix = trimmed_field(&form, "account_prefix");
    let domain = domain_or(&form, state::ACCOUNT_DOMAIN_DEFAULT);
    let password = form.get("password").cloned().unwrap_or_default();
    let (Some(prefix), true) = (prefix, !password.is_empty()) else {
        return test_fragment(Vec::new(), "请填写邮箱账号与密码", false);
    };

    let account = full_address(&prefix, &domain);
    let smtp_host = host_or(&form, "smtp_host", state::SMTP_HOST_DEFAULT);
    let smtp_port = port_or(&form, "smtp_port", SMTP_PORT_DEFAULT);
    let recv_host = host_or(&form, "recv_host", state::RECV_HOST_DEFAULT);
    let recv_port = port_or(&form, "recv_port", RECV_PORT_DEFAULT);

    let smtp = tester.smtp(&smtp_host, smtp_port, &account, &password);
    let imap = tester.imap(&recv_host, recv_port, &account, &password);
    if smtp.ok && imap.ok {
        let display = trimmed_field(&form, "display_name").unwrap_or_default();
        let smtp_port_s = smtp_port.to_string();
        let recv_port_s = recv_port.to_string();
        let pairs = [
            (keys::ACCOUNT_PREFIX, prefix.as_str()),
            (keys::ACCOUNT_DOMAIN, domain.as_str()),
            (keys::DISPLAY_NAME, display.as_str()),
            (keys::SMTP_HOST, smtp_host.as_str()),
            (keys::SMTP_PORT, smtp_port_s.as_str()),
            (keys::RECV_HOST, recv_host.as_str()),
            (keys::RECV_PORT, recv_port_s.as_str()),
        ];
        if let Err(e) = state.set_batch(&pairs) {
            return test_fragment(Vec::new(), &format!("保存失败：{e}"), false);
        }
        if let Err(e) = state.store_password(&account, &password) {
            return test_fragment(Vec::new(), &format!("密码保存失败：{e}"), false);
        }
        return Resp::hx_redirect("/welcome/done");
    }
    let tests = vec![test_view("发信通道", &smtp), test_view("收信通道", &imap)];
    test_fragment(tests, "未保存：两条通道需全部通过", false)
}

/// 设置·账号 tab：测试连接（SMTP）/测试收信（IMAP）
/// 密码取值：表单优先，留空读已保存凭据（设计 §8.2）
fn settings_account_test(
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

/// 设置·账号 tab：保存（密码留空 = 不变；账号变更自动迁移凭据）
fn settings_account_save(state: &AppState, req: &Req) -> Resp {
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

    if prefix != old_prefix || domain != old_domain {
        // 账号变更：迁移凭据（密码框有值则直接写新账号；无值则搬旧凭据）
        match new_password {
            Some(p) => {
                if let Err(e) = state.store_password(&new_account, &p) {
                    return account_pane(state, false, &format!("密码保存失败：{e}"));
                }
            }
            None => match state.retrieve_password(&full_address(&old_prefix, &old_domain)) {
                crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p) => {
                    if let Err(e) = state.store_password(&new_account, &p) {
                        return account_pane(state, false, &format!("密码迁移失败：{e}"));
                    }
                }
                crate::secrets::GetResult::Missing => {
                    return account_pane(state, false, "账号已修改，请输入密码后保存");
                }
            },
        }
        if !old_prefix.is_empty() {
            state.clear_password(&full_address(&old_prefix, &old_domain));
        }
    } else if let Some(p) = &new_password
        && let Err(e) = state.store_password(&new_account, p)
    {
        return account_pane(state, false, &format!("密码保存失败：{e}"));
    }

    let smtp_host = host_state_or(&form, state, keys::SMTP_HOST, state::SMTP_HOST_DEFAULT);
    let recv_host = host_state_or(&form, state, keys::RECV_HOST, state::RECV_HOST_DEFAULT);
    let smtp_port_s = port_state_or(&form, state, keys::SMTP_PORT, SMTP_PORT_DEFAULT).to_string();
    let recv_port_s = port_state_or(&form, state, keys::RECV_PORT, RECV_PORT_DEFAULT).to_string();
    let display = trimmed_field(&form, "display_name").unwrap_or_default();
    let pairs = [
        (keys::ACCOUNT_PREFIX, prefix.as_str()),
        (keys::ACCOUNT_DOMAIN, domain.as_str()),
        (keys::DISPLAY_NAME, display.as_str()),
        (keys::SMTP_HOST, smtp_host.as_str()),
        (keys::SMTP_PORT, smtp_port_s.as_str()),
        (keys::RECV_HOST, recv_host.as_str()),
        (keys::RECV_PORT, recv_port_s.as_str()),
    ];
    if let Err(e) = state.set_batch(&pairs) {
        return account_pane(state, false, &format!("保存失败：{e}"));
    }
    account_pane(state, true, "")
}

/// 设置·收件同步 tab：保存（参数在 M4 收件实施时生效）
fn settings_sync_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let d = defaults();
    let cur = |k: &str| state.get_or(k, &d).unwrap_or_default();

    let on = match form.get("sync_on_start") {
        Some(v) => v == "true",
        None => cur(keys::SYNC_ON_START) == "true",
    };
    let interval = match form.get("sync_interval_min") {
        Some(v) => match v.trim().parse::<u32>() {
            Ok(n) => n.to_string(),
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

/// 设置·存储管理 tab：保存（清理参数在 M5 清理器实施时生效）
fn settings_storage_save(state: &AppState, req: &Req) -> Resp {
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

// ---------- 上下文装配 ----------

fn frame_ctx(state: &AppState) -> FrameCtx {
    let d = defaults();
    let prefix = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let display = state.get_or(keys::DISPLAY_NAME, &d).unwrap_or_default();
    let full = if prefix.is_empty() {
        "未配置账号".to_string()
    } else {
        full_address(&prefix, &domain)
    };
    let avatar = display
        .chars()
        .next()
        .or(prefix.chars().next())
        .unwrap_or('✉')
        .to_string();
    let c = state.folder_counts();
    FrameCtx {
        display_name: if display.is_empty() {
            "未配置".into()
        } else {
            display
        },
        full_addr: full,
        avatar,
        inbox: c["inbox"],
        inbox_unread: c["inbox_unread"],
        all_mail: c["all"],
        sent: c["sent"],
        draft: c["draft"],
    }
}

fn settings_ctx(state: &AppState, tab: &str, saved: bool, error: &str) -> SettingsCtx {
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let counts = state.folder_counts();
    let (attach_bytes, attach_files) = state.attachments_stats();
    let db_bytes = state.db_size_bytes();
    let total = db_bytes + attach_bytes;
    let db_pct = (db_bytes * 100)
        .checked_div(total)
        .map(|v| (v as u32).min(100))
        .unwrap_or(0);
    SettingsCtx {
        frame: FrameCtx {
            display_name: if g(keys::DISPLAY_NAME).is_empty() {
                "未配置".into()
            } else {
                g(keys::DISPLAY_NAME)
            },
            full_addr: if g(keys::ACCOUNT_PREFIX).is_empty() {
                "未配置账号".into()
            } else {
                full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN))
            },
            avatar: g(keys::DISPLAY_NAME)
                .chars()
                .next()
                .or(g(keys::ACCOUNT_PREFIX).chars().next())
                .unwrap_or('✉')
                .to_string(),
            inbox: counts["inbox"],
            inbox_unread: counts["inbox_unread"],
            all_mail: counts["all"],
            sent: counts["sent"],
            draft: counts["draft"],
        },
        tab: tab.into(),
        smtp_host: g(keys::SMTP_HOST),
        smtp_port: g(keys::SMTP_PORT),
        account_prefix: g(keys::ACCOUNT_PREFIX),
        account_domain: g(keys::ACCOUNT_DOMAIN),
        display_name: g(keys::DISPLAY_NAME),
        recv_host: g(keys::RECV_HOST),
        recv_port: g(keys::RECV_PORT),
        degraded: state.secrets_degraded(&full_address(
            &g(keys::ACCOUNT_PREFIX),
            &g(keys::ACCOUNT_DOMAIN),
        )),
        saved,
        error: error.into(),
        sync_on_start: g(keys::SYNC_ON_START) == "true",
        sync_interval_min: g(keys::SYNC_INTERVAL_MIN),
        first_fetch_days: g(keys::FIRST_FETCH_DAYS),
        last_sync_label: "收件功能将在后续版本开放，以下参数保存后随之生效。".into(),
        data_dir: state.data_dir.display().to_string(),
        inline_max: g(keys::INLINE_ATTACH_MAX_MB),
        db_size: human_bytes(db_bytes),
        db_pct,
        attach_size: human_bytes(attach_bytes),
        attach_files,
        attach_pct: 100 - db_pct,
        clean_days: g(keys::CLEAN_SENT_BULK_DAYS),
        keep_inbox: g(keys::KEEP_INBOX_DAYS),
        keep_sent: g(keys::KEEP_SENT_DAYS),
        app_version: env!("CARGO_PKG_VERSION").into(),
        sqlite_version: state.sqlite_version(),
    }
}

fn account_pane(state: &AppState, saved: bool, error: &str) -> Resp {
    render_html(
        &pane_template("account"),
        &settings_ctx(state, "account", saved, error),
    )
}

fn sync_pane(state: &AppState, saved: bool) -> Resp {
    render_html(
        &pane_template("sync"),
        &settings_ctx(state, "sync", saved, ""),
    )
}

fn storage_pane(state: &AppState, saved: bool) -> Resp {
    render_html(
        &pane_template("storage"),
        &settings_ctx(state, "storage", saved, ""),
    )
}

fn test_fragment(tests: Vec<TestView>, note: &str, note_ok: bool) -> Resp {
    let ctx = TestResultCtx {
        tests,
        note: note.into(),
        note_ok,
    };
    render_html("test_result.html", &ctx)
}

// ---------- 渲染与静态资源 ----------

/// 渲染命名模板为 HTML 响应（整页与片段同构，仅模板不同）
fn render_html(name: &str, ctx: &impl Serialize) -> Resp {
    match templates::render(name, ctx) {
        Ok(html) => Resp::html(html),
        Err(err) => Resp::internal_error(format!("模板 {name} 渲染失败：{err}")),
    }
}

/// 静态资源：仅 GET，路径不含 `..`/空段；MIME 与字节由资源提供方给出
fn serve_asset<A: Assets>(assets: &A, req: &Req) -> Option<Resp> {
    if req.method != "GET" {
        return None;
    }
    let rel = req.path.strip_prefix('/')?;
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return None;
    }
    let (mime, bytes) = assets.get(rel)?;
    Some(Resp::asset(mime, bytes))
}

fn error_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<title>{title}</title></head><body style=\"font-family:system-ui,sans-serif;\
color:#475569;padding:32px;\"><h1 style=\"font-size:18px;color:#334155;\">{title}</h1>\
<p>{message}</p></body></html>"
    )
}

// ---------- 表单解析辅助 ----------

/// 解析 application/x-www-form-urlencoded 请求体
fn parse_form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// 取去空白后的字段值（空串视为 None）
fn trimmed_field(form: &HashMap<String, String>, name: &str) -> Option<String> {
    form.get(name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 账号域名字段：表单值优先（统一带前导 @ 存储），空则取 fallback
fn domain_or(form: &HashMap<String, String>, fallback: &str) -> String {
    let v = trimmed_field(form, "account_domain").unwrap_or_else(|| fallback.to_string());
    if v.starts_with('@') {
        v
    } else {
        format!("@{v}")
    }
}

/// 主机字段：表单值优先，空则取参数 fallback
fn host_or(form: &HashMap<String, String>, name: &str, fallback: &str) -> String {
    trimmed_field(form, name).unwrap_or_else(|| fallback.to_string())
}

/// 端口字段：表单值优先，空/非法取 fallback
fn port_or(form: &HashMap<String, String>, name: &str, fallback: u16) -> u16 {
    form.get(name)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(fallback)
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

const SMTP_PORT_DEFAULT: u16 = 465;
const RECV_PORT_DEFAULT: u16 = 993;

fn human_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.0} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 假资源表（路径 → (MIME, 内容)）
    struct MapAssets(HashMap<&'static str, (&'static str, &'static str)>);

    impl Assets for MapAssets {
        fn get(&self, path: &str) -> Option<(String, Vec<u8>)> {
            self.0
                .get(path)
                .map(|(mime, body)| ((*mime).to_string(), body.as_bytes().to_vec()))
        }
    }

    fn assets() -> MapAssets {
        let mut map = HashMap::new();
        map.insert("app.css", ("text/css; charset=utf-8", "body{color:#333}"));
        map.insert("htmx.min.js", ("text/javascript", "var htmx={}"));
        map.insert("../Cargo.toml", ("text/plain", "[package]")); // 验证防穿越守卫
        MapAssets(map)
    }

    /// 假连接测试器（避免单测走网络）
    struct FakeTester {
        smtp_ok: bool,
        imap_ok: bool,
    }

    impl ConnTester for FakeTester {
        fn smtp(&self, _host: &str, _port: u16, _account: &str, _password: &str) -> TestOutcome {
            TestOutcome {
                ok: self.smtp_ok,
                elapsed_ms: 3,
                detail: if self.smtp_ok {
                    String::new()
                } else {
                    "模拟失败".into()
                },
            }
        }

        fn imap(&self, _host: &str, _port: u16, _account: &str, _password: &str) -> TestOutcome {
            TestOutcome {
                ok: self.imap_ok,
                elapsed_ms: 4,
                detail: if self.imap_ok {
                    String::new()
                } else {
                    "模拟失败".into()
                },
            }
        }
    }

    fn temp_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mbx-router-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (AppState::open(&dir), dir)
    }

    fn cleanup(dir: &std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn req(method: &str, path: &str, body: &[u8]) -> Req {
        Req {
            method: method.to_string(),
            path: path.to_string(),
            is_htmx_request: false,
            body: body.to_vec(),
        }
    }

    fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut ser = form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            ser.append_pair(k, v);
        }
        ser.finish().into_bytes()
    }

    fn body(resp: &Resp) -> String {
        String::from_utf8(resp.body.clone()).expect("响应为 UTF-8 HTML")
    }

    const WIZARD_FORM: &[(&str, &str)] = &[
        ("account_prefix", "unit.router"),
        ("password", "pw-test"),
        ("display_name", "单测用户"),
        ("smtp_host", "smtp.test.cn"),
        ("smtp_port", "465"),
        ("recv_host", "imap.test.cn"),
        ("recv_port", "993"),
    ];

    #[test]
    fn fresh_root_shows_welcome() {
        let (state, dir) = temp_state("root-fresh");
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert_eq!(resp.status, 200);
        let html = body(&resp);
        assert!(html.contains("欢迎使用 Mailbox"), "未配置首启应渲染欢迎页");
        assert!(html.contains("开始配置"), "欢迎页应有强制配置入口");
        cleanup(&dir);
    }

    #[test]
    fn configured_root_shows_main_frame() {
        let (state, dir) = temp_state("root-cfg");
        state.set_batch(&[(keys::ACCOUNT_PREFIX, "demo")]).unwrap();
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(html.contains("企业邮箱客户端"), "已配置应渲染主界面框架");
        assert!(html.contains("还没有邮件"), "M1 主界面为空列表占位");
        assert!(html.contains("demo@chinatelecom.cn"), "侧栏应展示完整账号");
        cleanup(&dir);
    }

    #[test]
    fn wizard_setup_form_prefills_defaults() {
        let (state, dir) = temp_state("setup");
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/welcome/setup", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(html.contains("smtp.chinatelecom.cn"), "SMTP 主机应预填");
        assert!(html.contains("value=\"465\""), "SMTP 端口应预填 465");
        assert!(html.contains("imap.chinatelecom.cn"), "IMAP 主机应预填");
        assert!(html.contains("@chinatelecom.cn"), "后缀应固定展示");
        cleanup(&dir);
    }

    #[test]
    fn wizard_save_requires_account_and_password() {
        let (state, dir) = temp_state("wiz-empty");
        for pairs in [
            vec![("account_prefix", ""), ("password", "x")],
            vec![("account_prefix", "demo"), ("password", "")],
        ] {
            let resp = route(
                &state,
                &assets(),
                &req("POST", "/welcome/setup/save", &form(&pairs)),
                &FakeTester {
                    smtp_ok: true,
                    imap_ok: true,
                },
            );
            assert!(body(&resp).contains("请填写邮箱账号与密码"), "空字段应拦截");
        }
        assert!(!state.configured(), "不应有副作用");
        cleanup(&dir);
    }

    #[test]
    fn wizard_save_success_redirects_and_persists() {
        let (state, dir) = temp_state("wiz-ok");
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
            "成功保存应整页跳转完成页"
        );
        assert!(state.configured(), "配置应已落库");
        let d = defaults();
        assert_eq!(
            state.get_or(keys::SMTP_HOST, &d).as_deref(),
            Some("smtp.test.cn")
        );
        // 凭据走 keychain/keyfile，不落 settings 明文
        assert!(matches!(
            state.retrieve_password("unit.router@chinatelecom.cn"),
            crate::secrets::GetResult::Keychain(_) | crate::secrets::GetResult::Keyfile(_)
        ));
        state.clear_password("unit.router@chinatelecom.cn");
        cleanup(&dir);
    }

    #[test]
    fn wizard_save_aborts_when_any_channel_fails() {
        let (state, dir) = temp_state("wiz-fail");
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
            &FakeTester {
                smtp_ok: false,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(html.contains("未保存"), "失败应提示未保存");
        assert!(html.contains("发信通道"), "应展示发信通道结果");
        assert!(html.contains("模拟失败"), "应展示失败原因");
        assert!(!state.configured(), "测试未全过不得落库");
        cleanup(&dir);
    }

    #[test]
    fn wizard_done_shows_summary_and_guards_unconfigured() {
        let (state, dir) = temp_state("done");
        // 未配置直接访问 → 回欢迎页
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/welcome/done", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("欢迎使用 Mailbox"));
        // 配置后展示摘要
        let _ = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/welcome/done", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(
            html.contains("unit.router@chinatelecom.cn"),
            "完成页应展示完整账号"
        );
        assert!(html.contains("smtp.test.cn:465"), "完成页应展示发信服务器");
        state.clear_password("unit.router@chinatelecom.cn");
        cleanup(&dir);
    }

    #[test]
    fn settings_page_renders_account_pane() {
        let (state, dir) = temp_state("set-page");
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/settings", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(
            html.contains("id=\"settings-pane\""),
            "设置页应含 pane 容器"
        );
        assert!(html.contains("发信（SMTP）"), "默认打开账号 tab");
        assert!(html.contains("收件同步"), "tab 条应完整");
        // 直接 URL 指定 tab
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/settings/storage", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("数据与附件"), "URL 直达存储 tab");
        cleanup(&dir);
    }

    #[test]
    fn settings_pane_fragment_switch_without_layout() {
        let (state, dir) = temp_state("set-frag");
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/settings/panes/sync", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(!html.contains("<html"), "片段不含完整布局");
        assert!(html.contains("启动时自动收取"), "应渲染同步 pane 内容");
        // 非法 tab → 404
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/settings/panes/nope", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert_eq!(resp.status, 404, "非法 tab 应 404");
        cleanup(&dir);
    }

    #[test]
    fn settings_sync_save_roundtrip() {
        let (state, dir) = temp_state("sync-save");
        let resp = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/sync/save",
                &form(&[
                    ("sync_on_start", "true"),
                    ("sync_interval_min", "15"),
                    ("first_fetch_days", "90"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("✓ 已保存"), "保存后片段应回显已保存");
        let d = defaults();
        assert_eq!(
            state.get_or(keys::SYNC_INTERVAL_MIN, &d).as_deref(),
            Some("15")
        );
        assert_eq!(
            state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
            Some("90")
        );
        assert_eq!(
            state.get_or(keys::SYNC_ON_START, &d).as_deref(),
            Some("true")
        );
        // 非法输入保持现值
        let _ = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/sync/save",
                &form(&[
                    ("sync_on_start", "false"),
                    ("sync_interval_min", "abc"),
                    ("first_fetch_days", "x"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert_eq!(
            state.get_or(keys::SYNC_INTERVAL_MIN, &d).as_deref(),
            Some("15"),
            "非法间隔应保持现值"
        );
        assert_eq!(
            state.get_or(keys::FIRST_FETCH_DAYS, &d).as_deref(),
            Some("90"),
            "非法范围应保持现值"
        );
        assert_eq!(
            state.get_or(keys::SYNC_ON_START, &d).as_deref(),
            Some("false"),
            "开关本身可关"
        );
        cleanup(&dir);
    }

    #[test]
    fn settings_storage_save_roundtrip_with_blank_semantics() {
        let (state, dir) = temp_state("sto-save");
        let resp = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/storage/save",
                &form(&[
                    ("inline_attach_max_mb", "50"),
                    ("clean_sent_bulk_days", ""),
                    ("keep_inbox_days", "400"),
                    ("keep_sent_days", "x"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("✓ 已保存"));
        assert!(body(&resp).contains("50"), "片段应回显新上限");
        let d = defaults();
        assert_eq!(
            state.get_or(keys::INLINE_ATTACH_MAX_MB, &d).as_deref(),
            Some("50")
        );
        assert_eq!(
            state.get_or(keys::CLEAN_SENT_BULK_DAYS, &d).as_deref(),
            Some(""),
            "留空=永久保留"
        );
        assert_eq!(
            state.get_or(keys::KEEP_INBOX_DAYS, &d).as_deref(),
            Some("400")
        );
        assert_eq!(
            state.get_or(keys::KEEP_SENT_DAYS, &d).as_deref(),
            Some("180"),
            "非法值保持默认"
        );
        cleanup(&dir);
    }

    #[test]
    fn settings_account_save_keeps_credential_when_blank() {
        let (state, dir) = temp_state("acc-save");
        // 先经向导写入凭据
        let _ = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        // 同账号、密码留空保存：凭据不动
        let resp = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/account/save",
                &form(&[
                    ("account_prefix", "unit.router"),
                    ("password", ""),
                    ("display_name", "新名字"),
                    ("smtp_host", "smtp.new.cn"),
                    ("smtp_port", "465"),
                    ("recv_host", "imap.test.cn"),
                    ("recv_port", "993"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("✓ 已保存"));
        assert!(
            matches!(
                state.retrieve_password("unit.router@chinatelecom.cn"),
                crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p)
                    if p == "pw-test"
            ),
            "密码留空不应改动凭据"
        );
        let d = defaults();
        assert_eq!(
            state.get_or(keys::DISPLAY_NAME, &d).as_deref(),
            Some("新名字")
        );
        assert_eq!(
            state.get_or(keys::SMTP_HOST, &d).as_deref(),
            Some("smtp.new.cn")
        );
        state.clear_password("unit.router@chinatelecom.cn");
        cleanup(&dir);
    }

    #[test]
    fn wizard_save_with_custom_domain() {
        let (state, dir) = temp_state("wiz-domain");
        let mut pairs: Vec<(&str, &str)> = WIZARD_FORM.to_vec();
        pairs.push(("account_domain", "@custom.cn"));
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(&pairs)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "HX-Redirect" && v == "/welcome/done"),
            "自定义域名保存应跳转完成页"
        );
        let d = defaults();
        assert_eq!(
            state.get_or(keys::ACCOUNT_DOMAIN, &d).as_deref(),
            Some("@custom.cn"),
            "域名应落库"
        );
        assert!(
            matches!(
                state.retrieve_password("unit.router@custom.cn"),
                crate::secrets::GetResult::Keychain(_) | crate::secrets::GetResult::Keyfile(_)
            ),
            "凭据应存到自定义域名账号下"
        );
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/welcome/done", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(
            body(&resp).contains("unit.router@custom.cn"),
            "完成页应展示自定义域名账号"
        );
        state.clear_password("unit.router@custom.cn");
        cleanup(&dir);
    }

    #[test]
    fn settings_domain_change_migrates_credential() {
        let (state, dir) = temp_state("acc-domain-chg");
        // 先经向导写入凭据（默认 @chinatelecom.cn）
        let _ = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        // 设置页仅改域名、密码留空 → 凭据随迁
        let resp = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/account/save",
                &form(&[
                    ("account_prefix", "unit.router"),
                    ("account_domain", "@custom.cn"),
                    ("password", ""),
                    ("display_name", "单测用户"),
                    ("smtp_host", "smtp.test.cn"),
                    ("smtp_port", "465"),
                    ("recv_host", "imap.test.cn"),
                    ("recv_port", "993"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(body(&resp).contains("✓ 已保存"));
        assert!(
            matches!(
                state.retrieve_password("unit.router@custom.cn"),
                crate::secrets::GetResult::Keychain(p) | crate::secrets::GetResult::Keyfile(p)
                    if p == "pw-test"
            ),
            "旧凭据应迁移到新域名账号"
        );
        assert!(
            matches!(
                state.retrieve_password("unit.router@chinatelecom.cn"),
                crate::secrets::GetResult::Missing
            ),
            "旧账号凭据应清除"
        );
        let d = defaults();
        assert_eq!(
            state.get_or(keys::ACCOUNT_DOMAIN, &d).as_deref(),
            Some("@custom.cn")
        );
        state.clear_password("unit.router@custom.cn");
        cleanup(&dir);
    }

    #[test]
    fn settings_account_change_requires_password_when_no_credential() {
        let (state, dir) = temp_state("acc-chg");
        let resp = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/account/save",
                &form(&[
                    ("account_prefix", "someone.new"),
                    ("password", ""),
                    ("smtp_host", "smtp.test.cn"),
                    ("smtp_port", "465"),
                    ("recv_host", "imap.test.cn"),
                    ("recv_port", "993"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert!(
            body(&resp).contains("请输入密码后保存"),
            "无凭据可迁时应要求输入密码"
        );
        let d = defaults();
        assert_eq!(state.get_or(keys::ACCOUNT_PREFIX, &d), None, "不应有副作用");
        cleanup(&dir);
    }

    #[test]
    fn settings_account_test_uses_saved_credential() {
        let (state, dir) = temp_state("acc-test");
        let _ = route(
            &state,
            &assets(),
            &req("POST", "/welcome/setup/save", &form(WIZARD_FORM)),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        // 密码留空 → 读已保存凭据测试
        let resp = route(
            &state,
            &assets(),
            &req(
                "POST",
                "/settings/account/test-imap",
                &form(&[
                    ("account_prefix", "unit.router"),
                    ("password", ""),
                    ("recv_host", "imap.test.cn"),
                    ("recv_port", "993"),
                ]),
            ),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        let html = body(&resp);
        assert!(html.contains("收信通道"), "应返回收信通道结果");
        assert!(html.contains("✓"), "假测试器应成功");
        state.clear_password("unit.router@chinatelecom.cn");
        cleanup(&dir);
    }

    #[test]
    fn static_assets_served_with_mime() {
        let (state, dir) = temp_state("assets");
        let resp = route(
            &state,
            &assets(),
            &req("GET", "/app.css", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "text/css; charset=utf-8");
        assert_eq!(body(&resp), "body{color:#333}");

        // 非 GET 不走静态
        let resp = route(
            &state,
            &assets(),
            &req("POST", "/app.css", b""),
            &FakeTester {
                smtp_ok: true,
                imap_ok: true,
            },
        );
        assert_eq!(resp.status, 404);
        cleanup(&dir);
    }

    #[test]
    fn traversal_and_unknown_paths_are_404() {
        let (state, dir) = temp_state("404");
        for path in ["/../Cargo.toml", "/nope", "/emails/1"] {
            let resp = route(
                &state,
                &assets(),
                &req("GET", path, b""),
                &FakeTester {
                    smtp_ok: true,
                    imap_ok: true,
                },
            );
            assert_eq!(resp.status, 404, "{path} 应 404");
        }
        cleanup(&dir);
    }

    /// 视觉冒烟素材导出：真实路由渲染全部 M1 页面到 .temp/smoke-html/，
    /// 静态资源路径改写为相对引用，供浏览器加载与截图检查。
    /// 用法：cargo test -- --ignored （不参与常规测试套件）
    #[test]
    #[ignore = "仅视觉冒烟素材导出：cargo test dump_smoke_html -- --ignored"]
    fn dump_smoke_html() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.temp/smoke-html");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db_dir = root.join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let state = AppState::open(&db_dir);
        let t = FakeTester {
            smtp_ok: true,
            imap_ok: true,
        };
        let a = assets();

        let get = |path: &str| body(&route(&state, &a, &req("GET", path, b""), &t));
        let dump = |name: &str, html: String| {
            let html = html
                .replace("href=\"/app.css\"", "href=\"app.css\"")
                .replace("src=\"/htmx.min.js\"", "src=\"htmx.min.js\"")
                .replace("href=\"/favicon.svg\"", "href=\"favicon.svg\"");
            std::fs::write(root.join(name), html).unwrap();
        };

        // 未配置：欢迎页 + 向导表单
        dump("welcome.html", get("/"));
        dump("setup.html", get("/welcome/setup"));

        // 直接种子配置（不走向导路由，避免单测环境写 keychain）
        state
            .set_batch(&[
                (keys::ACCOUNT_PREFIX, "demo"),
                (keys::DISPLAY_NAME, "张伟"),
                (keys::SMTP_HOST, "smtp.chinatelecom.cn"),
                (keys::SMTP_PORT, "465"),
                (keys::RECV_HOST, "imap.chinatelecom.cn"),
                (keys::RECV_PORT, "993"),
            ])
            .unwrap();

        // 已配置：完成页 + 主界面 + 设置四 tab
        dump("done.html", get("/welcome/done"));
        dump("main.html", get("/"));
        dump("settings-account.html", get("/settings"));
        dump("settings-sync.html", get("/settings/sync"));
        dump("settings-storage.html", get("/settings/storage"));
        dump("settings-about.html", get("/settings/about"));

        // 测试连接片段演示页（成功 + 失败双态 + 未保存提示）
        let frag = test_fragment(
            vec![
                test_view(
                    "发信通道",
                    &TestOutcome {
                        ok: true,
                        elapsed_ms: 342,
                        detail: String::new(),
                    },
                ),
                test_view(
                    "收信通道",
                    &TestOutcome {
                        ok: false,
                        elapsed_ms: 1200,
                        detail: "连接超时".into(),
                    },
                ),
            ],
            "未保存：两条通道需全部通过",
            false,
        );
        let wrapper = format!(
            "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<link rel=\"stylesheet\" href=\"app.css\"></head>\
<body style=\"padding:48px;background:#f2f4f8\">\
<div class=\"setup-card\" style=\"max-width:560px;margin:0 auto\">\
<div class=\"test-row\">{}</div></div></body></html>",
            body(&frag)
        );
        dump("test-result-demo.html", wrapper);

        // 复制真实静态资源（与发行物同源）
        let ui_dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui-dist");
        for f in ["app.css", "htmx.min.js", "favicon.svg"] {
            std::fs::copy(ui_dist.join(f), root.join(f)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&db_dir);
    }
}
