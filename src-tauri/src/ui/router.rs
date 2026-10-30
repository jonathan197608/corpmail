//! 路由：把 `ui://` 请求映射为 HTML 页面/片段或静态资源响应（M1 版）
//!
//! - 非向导不可达原则：未完成首启配置时根路径渲染欢迎页
//! - 网络测试（SMTP/IMAP）经 `ConnTester` 注入：生产走 `conncheck` 真连，
//!   单测注入假实现（协议回调同步语义，测试期间阻塞可接受）
//! - htmx 片段响应：pane 片段（tab 切换/保存回显）与 test_result 片段（连接测试）
//! - 向导保存成功以 `HX-Redirect` 响应头驱动整页跳转（htmx 约定）

use super::settings::{
    settings_account_save, settings_account_test, settings_backup_delete, settings_page,
    settings_pane_fragment, settings_storage_clean, settings_storage_move, settings_storage_save,
    settings_sync_refetch, settings_sync_save,
};
use crate::conncheck::TestOutcome;
use crate::mail::{self, AttachView, EmailSummary, html_escape};
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
    /// 查询串（不含 `?`；M2 起 `/` 列表按 folder/page/unread 取参）
    pub query: String,
    /// 是否带 `HX-Request` 头（htmx 发起的片段请求）。
    /// 列表页按它区分整页/片段响应（片段=mail_list 单交换）
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
            query: request.uri().query().unwrap_or("").to_string(),
            is_htmx_request: request.headers().get("hx-request").is_some(),
            body: request.body().clone(),
        }
    }

    /// 取查询参数（首个命中；无则 None）
    pub fn query_param(&self, name: &str) -> Option<String> {
        form_urlencoded::parse(self.query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string())
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
pub(super) struct FrameCtx {
    pub(super) display_name: String,
    pub(super) full_addr: String,
    pub(super) avatar: String,
    pub(super) inbox: i64,
    pub(super) inbox_unread: i64,
    pub(super) all_mail: i64,
    pub(super) sent: i64,
    pub(super) draft: i64,
    /// 当前列表 folder（inbox|all|sent|draft；side.html 高亮与列表页标题用）
    pub(super) folder: String,
}

#[derive(Serialize)]
struct MainCtx {
    frame: FrameCtx,
    folder: String,
    folder_label: String,
    unread_only: bool,
    page: i64,
    pages: i64,
    total: i64,
    /// 分页链接前缀（含 folder/unread/q 参数；模板拼 page=N）
    page_base: String,
    /// 未读开关基址（仅 folder/q，无 unread：开启/恢复链接共用，防恢复链接自带 unread）
    plain_base: String,
    /// 当前检索词（空 = 普通列表；回显搜索框 + 空态文案区分）
    q: String,
    items: Vec<EmailSummary>,
}

/// 邮件详情页
#[derive(Serialize)]
struct EmailCtx {
    frame: FrameCtx,
    email: mail::EmailDetail,
    /// 页面轻提示（error query 回显，如重发被拒）
    notice: String,
}

/// 撰写页（新邮件 / 回复预填 / 草稿编辑共用）
#[derive(Serialize)]
struct ComposeCtx {
    frame: FrameCtx,
    draft_id: i64,
    in_reply_to: i64,
    to_addrs: String,
    cc_addrs: String,
    subject: String,
    body_text: String,
    attachments: Vec<AttachView>,
    /// 已入库附件 id 的 JSON 数组（keep_ids 隐藏域初值；JS 维护移除）
    keep_ids_json: String,
    saved: bool,
    error: String,
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
    /// 首次拉取范围（向导可设；与设置同步页一致）
    first_fetch_days: String,
    /// 数据保存目录（当前值，供输入框回显；留空=默认）
    data_dir: String,
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
    /// 若向导中迁移了数据目录：待重启生效的目录（done 页提示）
    pending_dir: Option<String>,
}

/// 连接测试结果片段
#[derive(Serialize)]
struct TestResultCtx {
    tests: Vec<TestView>,
    note: String,
    note_ok: bool,
}

/// 收件状态片段（顶栏收取按钮反馈区 + /emails/fetch/status 轮询共用）
#[derive(Serialize)]
struct FetchStatusCtx {
    running: bool,
    new_count: i64,
    error: String,
    last_sync_label: String,
    /// M6 分批进度：已入库 / 应拉总数（running 且 total>0 时展示）
    done: usize,
    total: usize,
}

#[derive(Serialize)]
pub(super) struct TestView {
    pub(super) label: String,
    pub(super) ok: bool,
    pub(super) elapsed_ms: u64,
    pub(super) detail: String,
}

pub(super) fn test_view(label: &str, o: &TestOutcome) -> TestView {
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
        ("GET", "/") => Some(list_root(state, req)),
        ("GET", "/compose") => Some(compose_page(state, req)),
        ("POST", "/compose/draft") => Some(compose_draft_save(state, req)),
        ("POST", "/compose/send") => Some(compose_send(state, req)),
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/resend") => {
            Some(email_resend(state, req))
        }
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/delete") => {
            Some(email_delete(state, req))
        }
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/read") => {
            Some(email_read(state, req))
        }
        ("GET", "/emails/fetch") => Some(fetch_start(state)),
        ("GET", "/emails/fetch/status") => Some(fetch_status(state, true)),
        ("POST", p) if p.starts_with("/emails/") && p.contains("/attachments/") => {
            Some(attachment_save(state, req))
        }
        ("GET", p)
            if p.starts_with("/emails/") && p.contains("/attachments/") && p.ends_with("/view") =>
        {
            Some(attachment_view(state, req))
        }
        ("GET", p) if p.starts_with("/emails/") => email_detail_page(state, req),
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
        ("POST", "/settings/sync/refetch") => Some(settings_sync_refetch(state, req)),
        ("POST", "/settings/storage/save") => Some(settings_storage_save(state, req)),
        ("POST", "/settings/storage/clean") => Some(settings_storage_clean(state)),
        ("POST", "/settings/storage/move") => Some(settings_storage_move(state, req)),
        ("POST", "/settings/storage/backup/delete") => Some(settings_backup_delete(state)),
        _ => None,
    }
}

fn valid_tab(tab: &str) -> bool {
    matches!(tab, "account" | "sync" | "storage" | "about")
}

// ---------- 页面 ----------

fn folder_label(folder: &str) -> &'static str {
    match folder {
        "inbox" => "收件箱",
        "sent" => "已发送",
        "draft" => "草稿",
        _ => "全部邮件",
    }
}

/// 主列表（GET /?folder=&page=&unread=&q=；q 非空时全文检索，无效词回退普通列表；
/// htmx 请求回 mail_list 片段，普通请求回整页）
fn list_root(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        // 首启未配置：配置为强制入口（设计 §8.1，无"稍后再说"）
        return render_html("welcome.html", &());
    }
    let folder = match req.query_param("folder").as_deref() {
        Some("all") | Some("sent") | Some("draft") => req.query_param("folder").unwrap(),
        _ => "inbox".to_string(),
    };
    let page = req
        .query_param("page")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let unread_only = req.query_param("unread").map(|v| v == "1").unwrap_or(false);
    let q_input = req.query_param("q").unwrap_or_default().trim().to_string();
    let q = mail::ListQuery {
        folder: folder.clone(),
        page,
        unread_only,
    };
    // 全文检索（§4.3）：有有效词走 FTS+BM25；空输入/全停用词/检索异常回退普通列表
    let (items, total) = if q_input.is_empty() {
        mail::list(state, &q).unwrap_or_default()
    } else {
        match mail::search(state, &q_input, &q) {
            Ok(Some(res)) => res,
            Ok(None) | Err(_) => mail::list(state, &q).unwrap_or_default(),
        }
    };
    let pages = (total + mail::PAGE_SIZE - 1) / mail::PAGE_SIZE;
    // plain_base：folder+q 基址（无 unread）——未读开关开启/恢复链接共用，
    // 恢复链接不得复用含 unread 的 page_base（否则永远跳不回全部）
    let mut plain_base = if folder == "inbox" {
        "/?".to_string()
    } else {
        format!("/?folder={folder}&")
    };
    if !q_input.is_empty() {
        plain_base = format!("{plain_base}q={}&", urlencode(&q_input));
    }
    let mut page_base = plain_base.clone();
    if unread_only {
        page_base = format!("{page_base}unread=1&");
    }
    let ctx = MainCtx {
        frame: frame_ctx(state, &folder),
        folder: folder.clone(),
        folder_label: folder_label(&folder).into(),
        unread_only,
        page,
        pages,
        total,
        page_base,
        plain_base,
        q: q_input,
        items,
    };
    if req.is_htmx_request {
        render_html("mail_list.html", &ctx)
    } else {
        render_html("main.html", &ctx)
    }
}

/// 详情页（GET /emails/{id}；打开即本地置已读，IMAP 服务器回写为 M4 §6.1）
fn email_detail_page(state: &AppState, req: &Req) -> Option<Resp> {
    // 无效路径先 404（与未配置欢迎页语义区分），再走"配置为强制入口"分支
    let id = path_id(&req.path, "/emails/")?;
    if !state.configured() {
        return Some(render_html("welcome.html", &()));
    }
    // 打开详情即阅读：本地置已读 + 服务器 \Seen 异步回写（M4 §6.1）
    mark_read_and_sync_seen(state, id);
    let email = mail::get(state, id)?;
    let folder = email.folder.clone();
    // query_param 已做百分号解码，勿再 urldecode（二次解码会破坏含 %/+ 的错误文案）
    let notice = req.query_param("error").unwrap_or_default();
    Some(render_html(
        "email.html",
        &EmailCtx {
            frame: frame_ctx(state, &folder),
            email,
            notice,
        },
    ))
}

/// 删除（POST /emails/{id}/delete；§5.3 硬删）——详情页按钮，删完整页回列表
fn email_delete(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    let folder = mail::get(state, id)
        .map(|d| d.folder)
        .unwrap_or_else(|| "inbox".into());
    match mail::delete(state, id) {
        Ok(_) => Resp::hx_redirect(&format!("/?folder={folder}")),
        Err(e) => Resp::hx_redirect(&format!(
            "/emails/{id}?error={}",
            urlencode(&format!("删除失败：{e}"))
        )),
    }
}

/// 标记已读（POST /emails/{id}/read）：本地置 0 + 服务器 \Seen 异步回写（§6.1，M4）
fn email_read(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    mark_read_and_sync_seen(state, id);
    Resp::html(String::new())
}

/// 本地置已读（从未读翻转时）+ 服务器 \Seen 异步回写。
/// 参数快照在调用线程（协议回调内），网络回写交独立线程，不阻塞回调。
fn mark_read_and_sync_seen(state: &AppState, id: i64) {
    if !mail::mark_read(state, id) {
        return; // 原本已读：无需回写
    }
    let Some(p) = mail::seen_params_of(state, id) else {
        return; // 非 in 邮件或凭据缺失：仅本地已读
    };
    std::thread::spawn(move || {
        let _ = mail::MailFetcher::mark_seen(
            &mail::RealFetcher,
            &p.host,
            p.port,
            &p.account,
            &p.password,
            p.uidvalidity,
            p.uid,
        );
    });
}

/// 手动收取（GET /emails/fetch，§6.2）：启动一轮（已在收件中则忽略）后回进行中片段
fn fetch_start(state: &AppState) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    state.start_fetch();
    fetch_status(state, false)
}

/// 收件状态片段（GET /emails/fetch/status 轮询）：进行中每 2s 自轮询；
/// 刚完成时消费标记并回 HX-Refresh 整页刷新（列表/计数全新）
fn fetch_status(state: &AppState, consume: bool) -> Resp {
    if !state.configured() {
        // 未配置：浮条留空（.fetch-status:empty 隐藏），避免欢迎页出现"没有新邮件"
        return Resp::html(
            r#"<div id="fetch-status" class="fetch-status fetch-status-global"></div>"#.into(),
        );
    }
    let s = state.fetch_snapshot();
    let d = defaults();
    let last_sync = state
        .get_or(keys::LAST_SYNC_AT, &d)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let ctx = FetchStatusCtx {
        running: s.running,
        new_count: s.new_count,
        error: s.last_error.clone(),
        last_sync_label: if last_sync > 0 {
            format!("上次收取：{}", mail::time_full(last_sync))
        } else {
            "尚未收取".into()
        },
        done: s.fetch_done,
        total: s.fetch_total,
    };
    let mut resp = render_html("fetch_status.html", &ctx);
    if consume && s.just_finished {
        state.clear_fetch_finish();
        resp.extra_headers
            .push(("HX-Refresh".into(), "true".into()));
    }
    resp
}

/// 撰写页（GET /compose：空表单 / ?reply_to=N 回复预填 / ?draft_id=M 草稿编辑）
fn compose_page(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let saved = req.query_param("saved").map(|v| v == "1").unwrap_or(false);
    // query_param 已解码，勿再 urldecode（二次解码会破坏含 %/+ 的错误文案）
    let error = req.query_param("error").unwrap_or_default();
    let mut ctx = ComposeCtx {
        frame: frame_ctx(state, "draft"),
        draft_id: 0,
        in_reply_to: 0,
        to_addrs: String::new(),
        cc_addrs: String::new(),
        subject: String::new(),
        body_text: String::new(),
        attachments: Vec::new(),
        keep_ids_json: "[]".into(),
        saved,
        error,
    };
    if let Some(id) = req
        .query_param("draft_id")
        .and_then(|v| v.parse::<i64>().ok())
    {
        if let Some(draft) = mail::get(state, id).filter(|d| d.folder == "draft") {
            ctx.draft_id = id;
            ctx.in_reply_to = draft.in_reply_to.unwrap_or(0);
            ctx.to_addrs = draft.to_addrs;
            ctx.cc_addrs = draft.cc_addrs;
            ctx.subject = draft.subject;
            ctx.body_text = draft.body_text;
            ctx.attachments = draft.attachments;
            ctx.keep_ids_json =
                serde_json::to_string(&ctx.attachments.iter().map(|a| a.id).collect::<Vec<_>>())
                    .unwrap_or_else(|_| "[]".into());
        }
    } else if let Some(rid) = req
        .query_param("reply_to")
        .and_then(|v| v.parse::<i64>().ok())
        && let Some(src) = mail::get(state, rid)
    {
        ctx.in_reply_to = rid;
        ctx.to_addrs = src.from_addr.clone();
        ctx.subject = if src.subject.starts_with("Re:") {
            src.subject.clone()
        } else {
            format!("Re: {}", src.subject)
        };
        let quoted = src
            .body_text
            .lines()
            .map(|l| format!("· {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.body_text = format!("\n\n----- 原文 -----\n{quoted}\n");
    }
    render_html("compose.html", &ctx)
}

/// 存/覆盖草稿（POST /compose/draft）：表单含 keep_ids/attach_paths（JSON 数组）
fn compose_draft_save(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    match save_compose_form(state, req) {
        Ok(id) => Resp::hx_redirect(&format!("/compose?draft_id={id}&saved=1")),
        Err((draft_id, e)) => redirect_compose(draft_id, &e),
    }
}

/// 撰写表单公共保存（存草稿与发送共用 §9）：落草稿 → keep 裁剪 → 新附件追加。
/// 失败返回 (原 draft_id, 原因) 供回撰写页提示。
fn save_compose_form(state: &AppState, req: &Req) -> Result<i64, (i64, String)> {
    let form = parse_form(&req.body);
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let draft_id = form_i64(&form, "draft_id");
    let input = mail::DraftInput {
        id: draft_id,
        from_addr: full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN)),
        from_name: g(keys::DISPLAY_NAME),
        to_addrs: trimmed_field(&form, "to_addrs").unwrap_or_default(),
        cc_addrs: trimmed_field(&form, "cc_addrs").unwrap_or_default(),
        subject: form.get("subject").cloned().unwrap_or_default(),
        body_text: form.get("body_text").cloned().unwrap_or_default(),
        in_reply_to: form_i64(&form, "in_reply_to"),
    };
    let keep_ids = form_json_i64s(&form, "keep_ids");
    let paths = form_json_strings(&form, "attach_paths");
    let id = mail::save_draft(state, &input).map_err(|e| (draft_id.unwrap_or(0), e))?;
    mail::remove_attachments_except(state, id, &keep_ids)
        .map_err(|e| (id, format!("附件裁剪失败：{e}")))?;
    if !paths.is_empty() {
        mail::add_attachments(state, id, &paths).map_err(|e| (id, format!("附件保存失败：{e}")))?;
    }
    Ok(id)
}

/// 发送（POST /compose/send §9）：先存最新内容 → 原子排队（重复提交 0 行，幂等 §7）
/// → 入发送队列 → 跳详情页轮询至终态（§2 异步原则）
fn compose_send(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    match save_compose_form(state, req) {
        Ok(id) => {
            if mail::enqueue_draft(state, id) {
                state.enqueue_send(id);
            }
            Resp::hx_redirect(&format!("/emails/{id}"))
        }
        Err((draft_id, e)) => redirect_compose(draft_id, &e),
    }
}

/// 重发入口（POST /emails/{id}/resend §9）：failed → draft 回撰写页编辑后重发
fn email_resend(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    if mail::reopen_failed(state, id) {
        Resp::hx_redirect(&format!("/compose?draft_id={id}"))
    } else {
        Resp::hx_redirect(&format!(
            "/emails/{id}?error={}",
            urlencode("仅发送失败的邮件可重发")
        ))
    }
}

fn redirect_compose(draft_id: i64, error: &str) -> Resp {
    Resp::hx_redirect(&format!(
        "/compose?draft_id={draft_id}&error={}",
        urlencode(error)
    ))
}

/// 附件预览（GET /emails/{id}/attachments/{aid}/view，M5）：解压后按原始 MIME 内联返回。
/// 安全约束：邮件头 MIME 不可信（可含控制字符/伪造类型）→ 白名单校验后回退 octet-stream；
/// text 类强制 text/plain（防 HTML 脚本执行）；SVG 不在图片白名单（顶层渲染可执行脚本）；
/// 非内联类型附 Content-Disposition: attachment（强制下载）。
fn attachment_view(state: &AppState, req: &Req) -> Resp {
    let Some((_, aid)) = path_attach(&req.path) else {
        return Resp::not_found();
    };
    match mail::attachment_bytes(state, aid) {
        Some((view, bytes)) => {
            let mime = view.mime.trim().to_ascii_lowercase();
            let (content_type, inline) = if view.preview_kind == "text" {
                ("text/plain; charset=utf-8".to_string(), true)
            } else if view.preview_kind == "img"
                && matches!(
                    mime.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
                )
            {
                (mime, true)
            } else if view.preview_kind == "pdf" {
                ("application/pdf".to_string(), true)
            } else {
                ("application/octet-stream".to_string(), false)
            };
            let mut resp = Resp {
                status: 200,
                content_type,
                cache_control: "no-store",
                body: bytes,
                extra_headers: Vec::new(),
            };
            resp.extra_headers
                .push(("X-Content-Type-Options".into(), "nosniff".into()));
            if !inline {
                resp.extra_headers
                    .push(("Content-Disposition".into(), "attachment".into()));
            }
            resp
        }
        None => Resp::not_found(),
    }
}

fn attachment_save(state: &AppState, req: &Req) -> Resp {
    let Some((_, aid)) = path_attach(&req.path) else {
        return Resp::not_found();
    };
    let form = parse_form(&req.body);
    let Some(dest) = trimmed_field(&form, "dest") else {
        return Resp::html(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">未选择保存位置</div>"#.into(),
        );
    };
    match mail::save_attachment_to(state, aid, &dest) {
        Ok(()) => Resp::html(format!(
            r#"<div id="toast" class="toast ok" hx-swap-oob="true">已另存到 {}</div>"#,
            html_escape(&dest)
        )),
        Err(e) => Resp::html(format!(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">{}</div>"#,
            html_escape(&e)
        )),
    }
}

// ---------- M2 辅助 ----------

/// 路径首段正整数 id（/emails/{id}...）
fn path_id(path: &str, prefix: &str) -> Option<i64> {
    path.strip_prefix(prefix)?
        .split('/')
        .next()?
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
}

/// 附件路径二元组（/emails/{id}/attachments/{aid}）
fn path_attach(path: &str) -> Option<(i64, i64)> {
    let rest = path.strip_prefix("/emails/")?;
    let mut it = rest.split('/');
    let id = it.next()?.parse::<i64>().ok()?;
    if it.next()? != "attachments" {
        return None;
    }
    let aid = it.next()?.parse::<i64>().ok()?;
    Some((id, aid))
}

fn form_i64(form: &HashMap<String, String>, name: &str) -> Option<i64> {
    form.get(name)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
}

/// 表单 JSON 数组字段 → i64 列表（空/缺省 → 空）
fn form_json_i64s(form: &HashMap<String, String>, name: &str) -> Vec<i64> {
    form.get(name)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

/// 表单 JSON 数组字段 → 字符串列表（附件路径）
fn form_json_strings(form: &HashMap<String, String>, name: &str) -> Vec<String> {
    form.get(name)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

fn urlencode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
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
        first_fetch_days: g(keys::FIRST_FETCH_DAYS),
        data_dir: state.data_dir.display().to_string(),
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
        pending_dir: state.pending_data_dir(),
    };
    render_html("done.html", &ctx)
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
        // 首次邮件拉取范围（7/30/90/全部=0，非法回落 30）
        let first_fetch = match form.get("first_fetch_days") {
            Some(v) if matches!(v.as_str(), "7" | "30" | "90" | "0") => v.clone(),
            _ => "30".to_string(),
        };
        // 数据保存目录：非空且不同于当前时迁移（迁移在账号落库前，失败不产生半保存态）
        let data_dir_input = trimmed_field(&form, "data_dir").unwrap_or_default();
        if !data_dir_input.is_empty() {
            let cur = state
                .data_dir
                .to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string();
            let want = data_dir_input
                .trim()
                .trim_end_matches(['/', '\\'])
                .to_string();
            if want != cur
                && let Err(e) = state.move_data_dir(&data_dir_input)
            {
                return test_fragment(Vec::new(), &format!("数据目录设置失败：{e}"), false);
            }
        }
        let display = trimmed_field(&form, "display_name").unwrap_or_default();
        let smtp_port_s = smtp_port.to_string();
        let recv_port_s = recv_port.to_string();
        let pairs: Vec<(&str, &str)> = vec![
            (keys::ACCOUNT_PREFIX, prefix.as_str()),
            (keys::ACCOUNT_DOMAIN, &domain),
            (keys::DISPLAY_NAME, display.as_str()),
            (keys::SMTP_HOST, smtp_host.as_str()),
            (keys::SMTP_PORT, smtp_port_s.as_str()),
            (keys::RECV_HOST, recv_host.as_str()),
            (keys::RECV_PORT, recv_port_s.as_str()),
            (keys::FIRST_FETCH_DAYS, first_fetch.as_str()),
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

// ---------- 上下文装配 ----------

/// 侧栏上下文构造（frame_ctx 与设置页共用；账号三要素由调用方预取，避免重复读配置）
pub(super) fn frame_of(
    prefix: &str,
    domain: &str,
    display: &str,
    folder: &str,
    counts: &HashMap<&'static str, i64>,
) -> FrameCtx {
    FrameCtx {
        display_name: if display.is_empty() {
            "未配置".into()
        } else {
            display.to_string()
        },
        full_addr: if prefix.is_empty() {
            "未配置账号".into()
        } else {
            full_address(prefix, domain)
        },
        avatar: display
            .chars()
            .next()
            .or(prefix.chars().next())
            .unwrap_or('✉')
            .to_string(),
        inbox: counts["inbox"],
        inbox_unread: counts["inbox_unread"],
        all_mail: counts["all"],
        sent: counts["sent"],
        draft: counts["draft"],
        folder: folder.into(),
    }
}

fn frame_ctx(state: &AppState, folder: &str) -> FrameCtx {
    let d = defaults();
    let prefix = state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let display = state.get_or(keys::DISPLAY_NAME, &d).unwrap_or_default();
    let counts = state.folder_counts();
    frame_of(&prefix, &domain, &display, folder, &counts)
}

pub(super) fn test_fragment(tests: Vec<TestView>, note: &str, note_ok: bool) -> Resp {
    let ctx = TestResultCtx {
        tests,
        note: note.into(),
        note_ok,
    };
    render_html("test_result.html", &ctx)
}

// ---------- 渲染与静态资源 ----------

/// 渲染命名模板为 HTML 响应（整页与片段同构，仅模板不同）
pub(super) fn render_html(name: &str, ctx: &impl Serialize) -> Resp {
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
    // 转义防注入：错误文案可能内嵌外部数据（如库错误串）
    let title = html_escape(title);
    let message = html_escape(message);
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<title>{title}</title></head><body style=\"font-family:system-ui,sans-serif;\
color:#475569;padding:32px;\"><h1 style=\"font-size:18px;color:#334155;\">{title}</h1>\
<p>{message}</p></body></html>"
    )
}

// ---------- 表单解析辅助 ----------

/// 解析 application/x-www-form-urlencoded 请求体
pub(super) fn parse_form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// 取去空白后的字段值（空串视为 None）
pub(super) fn trimmed_field(form: &HashMap<String, String>, name: &str) -> Option<String> {
    form.get(name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 账号域名字段：表单值优先（统一带前导 @ 存储），空则取 fallback
pub(super) fn domain_or(form: &HashMap<String, String>, fallback: &str) -> String {
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

pub(super) const SMTP_PORT_DEFAULT: u16 = 465;
pub(super) const RECV_PORT_DEFAULT: u16 = 993;

pub(super) fn human_bytes(b: u64) -> String {
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
#[path = "../tests/ui_router.rs"]
mod tests;
