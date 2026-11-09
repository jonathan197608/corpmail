//! 路由：把 `ui://` 请求映射为 HTML 页面/片段或静态资源响应（M1 版）
//!
//! - 非向导不可达原则：未完成首启配置时根路径渲染欢迎页
//! - 网络测试（SMTP/IMAP）经 `ConnTester` 注入：生产走 `conncheck` 真连，
//!   单测注入假实现（协议回调同步语义，测试期间阻塞可接受）
//! - htmx 片段响应：pane 片段（tab 切换/保存回显）与 test_result 片段（连接测试）
//! - 向导保存成功以 `HX-Redirect` 响应头驱动整页跳转（htmx 约定）

use super::settings::{
    settings_account_save, settings_account_test, settings_backup_delete, settings_ctx,
    settings_page, settings_pane_fragment, settings_storage_clean, settings_storage_move,
    settings_storage_save, settings_sync_refetch, settings_sync_save,
};
use crate::conncheck::TestOutcome;
use crate::mail::{self, AttachView, EmailSummary, html_escape};
use crate::state::{self, AppState, defaults, full_address, keys};
use crate::templates;
use crate::ui::Assets;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;
use url::form_urlencoded;

/// 连接测试注入点（生产实现真连服务器；单测注入假实现避免网络依赖）。
/// Send + Sync：向导保存时 SMTP/IMAP 双通道并行测试（std::thread::scope 借用）
pub trait ConnTester: Send + Sync {
    fn smtp(
        &self,
        host: &str,
        port: u16,
        mode: crate::net::TlsMode,
        account: &str,
        password: &str,
    ) -> TestOutcome;
    fn imap(
        &self,
        host: &str,
        port: u16,
        mode: crate::net::TlsMode,
        account: &str,
        password: &str,
    ) -> TestOutcome;
}

/// 生产实现：直连真实服务器（设计 §8.1 双通道测试）
pub struct RealTester;

impl ConnTester for RealTester {
    fn smtp(
        &self,
        host: &str,
        port: u16,
        mode: crate::net::TlsMode,
        account: &str,
        password: &str,
    ) -> TestOutcome {
        crate::conncheck::smtp_test(host, port, mode, account, password)
    }

    fn imap(
        &self,
        host: &str,
        port: u16,
        mode: crate::net::TlsMode,
        account: &str,
        password: &str,
    ) -> TestOutcome {
        crate::conncheck::imap_test(host, port, mode, account, password)
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

    /// JSON 响应（联系人建议等前端 fetch 接口）
    fn json(body: String) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8".into(),
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

    /// 附带 HX-Trigger 事件（htmx 收到后派发同名 body 事件，
    /// app.js 监听 mbx:fetchDone 做侧栏+列表局部刷新）
    fn with_event(mut self, event: &str) -> Self {
        self.extra_headers
            .push(("HX-Trigger".into(), format!("{{\"{event}\":true}}")));
        self
    }

    /// 同 with_event，但事件携带详情载荷（htmx 语义：{"事件名": {…}}
    /// → 监听回调从 ev.detail 取值；M2-1 mbx:starDone 回传翻转后的星标态）
    fn with_event_detail(mut self, event: &str, detail: &str) -> Self {
        self.extra_headers
            .push(("HX-Trigger".into(), format!("{{\"{event}\":{detail}}}")));
        self
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

/// 收件人地址片段（详情页按地址逐个标注“外部”）
#[derive(Serialize)]
struct AddrChip {
    text: String,
    external: bool,
}

/// 侧栏（主界面/设置页共用）
#[derive(Serialize)]
pub(super) struct FrameCtx {
    pub(super) display_name: String,
    pub(super) full_addr: String,
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
    /// 只看外部发件人
    external_only: bool,
    /// 未读/外部开关链接（各自取反、保留另一个筛选与检索词）
    unread_href: String,
    external_href: String,
    /// 当前列表完整查询串（folder/unread/external/q/page）：详情页链接携带，
    /// 「返回列表」按原样还原过滤状态
    list_query: String,
    page: i64,
    pages: i64,
    total: i64,
    /// 每页条数（当前生效值：URL per 或 settings；顶栏选择器回显）
    per: i64,
    /// 分页链接前缀（含 folder/unread/external/q/per 参数；模板拼 page=N）
    page_base: String,
    /// 当前检索词（空 = 普通列表；回显搜索框 + 空态文案区分）
    q: String,
    /// 排序键（A2；模板回显下拉，非默认值才进 URL）
    sort: String,
    /// 高级检索回显（B1）：发件人包含 / 起止日期（YYYY-MM-DD）/ 仅看附件
    from_filter: String,
    df: String,
    dt: String,
    has_attach: bool,
    items: Vec<EmailSummary>,
    /// 会话视图（M2-2）：mode=thread 时为 true；只在收件箱/全部/搜索下可用
    is_thread: bool,
    /// 会话列表行（is_thread 时渲染，否则为空）
    threads: Vec<mail::ThreadSummary>,
    /// 会话/单封视图切换链接（保留 folder/unread/external/q，翻页重置）
    thread_href: String,
}

/// 邮件详情页
#[derive(Serialize)]
struct EmailCtx {
    frame: FrameCtx,
    email: mail::EmailDetail,
    /// 「返回列表」链接：还原进入详情前的过滤状态（未读/外部/搜索/页码）
    back_href: String,
    /// 同一列表上下文的查询串（不含 `?`）：回复/转发/删除操作携带，
    /// 使撰写页返回与删除回跳同样保持原过滤与翻页
    list_query: String,
    /// 发件人是否外部（详情页发件人旁「外部」标记）
    from_external: bool,
    /// 收件人/抄送/密送按地址拆开，外部地址逐个标注
    to_chips: Vec<AddrChip>,
    cc_chips: Vec<AddrChip>,
    /// 密送 chips（仅自己发出的邮件有值；收件恒空不渲染）
    bcc_chips: Vec<AddrChip>,
    /// 页面轻提示（error query 回显，如重发被拒）
    notice: String,
    /// B4 导出 .eml 的推荐文件名（主题清洗后 + .eml）
    export_name: String,
    /// 导出 Markdown 的推荐文件名（与 .eml 同源清洗，扩展名 .md）
    export_md_name: String,
}

/// 源码页（A5）：排查正文乱码、HTML 表格丢失、投递链路问题。
/// part = html（净化后 HTML 源码）/ text（纯文本）/ headers（RFC 5322 原始信头）。
#[derive(Serialize)]
struct SourceCtx {
    frame: FrameCtx,
    email_id: i64,
    subject: String,
    part: String,
    /// 当前视图内容（模板内 `<pre>` 转义渲染）
    content: String,
    chars: usize,
    lines: usize,
    /// 当前视图为空时的说明文案
    empty_hint: String,
    back_href: String,
    list_query: String,
}

/// 会话详情页（M2-2）：一个 thread_key 归组的往来邮件，时间正序串列
#[derive(Serialize)]
struct ThreadCtx {
    frame: FrameCtx,
    detail: mail::ThreadDetail,
    /// 所属文件夹（取组内最新一封；frame 侧栏高亮 + 返回列表目标）
    folder: String,
    back_href: String,
}

/// 撰写页（新邮件 / 回复预填 / 草稿编辑共用）
#[derive(Serialize)]
struct ComposeCtx {
    frame: FrameCtx,
    draft_id: i64,
    in_reply_to: i64,
    /// 回复全部（标题区分；预填收件人=发件人+收件人+抄送，去掉自己）
    reply_all: bool,
    to_addrs: String,
    cc_addrs: String,
    /// 密送（草稿/重新编辑回填；回复/转发不继承——原邮件的密送方本就不可知）
    bcc_addrs: String,
    subject: String,
    body_text: String,
    /// HTML 编辑模式的正文源码（草稿回填用；仅 body_mode=="html" 时提交）
    body_html: String,
    /// 正文模式：plain | html
    body_mode: String,
    attachments: Vec<AttachView>,
    /// 已入库附件 id 的 JSON 数组（keep_ids 隐藏域初值；JS 维护移除）
    keep_ids_json: String,
    /// 「返回列表」链接：保持来源列表的过滤/翻页（无来源时回默认列表）
    back_href: String,
    /// 来源列表上下文（folder/unread/external/q/page）：随表单透传，
    /// 发送/存草稿后回详情页或撰写页时保持过滤状态（最后一环，2026-11-03）
    list_query: String,
    saved: bool,
    error: String,
    /// 本次被跳过的 0 字节文件数（附件选择误选空文件时提示，不阻断整单保存）
    skipped: usize,
    /// 转发来源邮件 id（0 = 非转发；首次保存时复制其附件到新草稿）
    forward_from: i64,
    /// 被转发邮件的附件数（页面提示"保存后携带"用）
    forward_attach_cnt: i64,
    /// 定时发送时刻（M4-2，datetime-local 控件值 `YYYY-MM-DDTHH:MM`；空 = 非定时）。
    /// 供控件回显 + 隐藏域初值（提交时服务端同一字符串再解析）
    sched_local: String,
    /// 刚撤销发送（A1）：显示「已撤销，可继续编辑」提示
    undone: bool,
    /// 邮件模板（B2）：正文上方「插入模板」下拉的数据源
    templates: Vec<mail::templates::Template>,
    /// 作为附件转发（B6）：源邮件 id（0 = 非此模式）；首次保存时把源邮件
    /// 生成为 .eml 并以 message/rfc822 附件加入草稿
    attach_eml_from: i64,
    /// 源邮件主题（页面提示「将携带 xxx.eml」用；空 = 无提示）
    attach_eml_name: String,
}

/// 首启向导·账号配置页（未配置字段为空，预填服务器默认值）
#[derive(Serialize)]
struct SetupCtx {
    account_prefix: String,
    account_domain: String,
    display_name: String,
    smtp_host: String,
    smtp_port: String,
    smtp_security: String,
    recv_host: String,
    recv_port: String,
    recv_security: String,
    /// 首次拉取范围（向导可设；与设置同步页一致）
    first_fetch_days: String,
    /// 数据保存目录（当前值，供输入框回显；留空=默认）
    data_dir: String,
    /// 凭据保管说明（按编译平台自适应：Windows 凭据管理器 / macOS 钥匙串）
    cred_note: String,
    /// 账号域名为 chinatelecom.cn：客户端专用密码/拉取范围需 OA 预先设置，展示指引
    is_ct_domain: bool,
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
    /// 首设直切（或已生效的）自定义数据目录：无重启，done 页展示当前目录
    dir_changed: Option<String>,
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
    /// 剩余时间预估文案（如 "约 3 分钟"）；空 = 不显示。
    /// 条件：running、有进度计时、且预估剩余 > 1 分钟——秒级完成不
    /// 打扰，短估跳变也不值得展示（需求：预估超过 1 分钟才显示）
    eta_label: String,
    /// 刚完成（片段根节点 data-just-finished 标记）：app.js 原生 fetch
    /// 看门狗据此兜底整页刷新——htmx 轮询链的触发器依赖元素换出后重建，
    /// 任一环节断裂即永不刷新；标记同时随 HX-Refresh 头下发，双通道任一生效
    just_finished: bool,
    /// 当前同步阶段（sent=发件箱 / inbox=收件箱 / 空）
    phase: String,
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
        // M3-1 草稿自动保存：与存草稿同一保存路径，返回轻量 JSON（无跳转）
        ("POST", "/compose/autosave") => Some(compose_autosave(state, req)),
        ("POST", "/compose/send") => Some(compose_send(state, req)),
        // A1 撤销发送：窗口内清 scheduled_at 回撰写页；窗口外提示已发出
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/undo-send") => {
            Some(email_undo_send(state, req))
        }
        // B4 导出 .eml（前端选保存位置后 POST）
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/export-eml") => {
            Some(email_export_eml(state, req))
        }
        // 导出 Markdown（供 AI 分析；前端选保存位置后 POST）
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/export-md") => {
            Some(email_export_md(state, req))
        }
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/resend") => {
            Some(email_resend(state, req))
        }
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/delete") => {
            Some(email_delete(state, req))
        }
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/read") => {
            Some(email_read(state, req))
        }
        // M2-1 星标翻转：列表/详情页星标按钮（hs-swap=none，事件驱动后续处理）
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/star") => {
            Some(email_star(state, req))
        }
        // A5 源码排查页：必须在详情通配之前（/emails/{id}/source 多段路径）
        ("GET", p) if p.starts_with("/emails/") && p.ends_with("/source") => {
            email_source(state, req)
        }
        // A5 配套：详情页「标为未读」留待办（跳回列表）
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/unread") => {
            Some(email_unread(state, req))
        }
        ("POST", "/emails/batch") => Some(emails_batch(state, req)),
        ("GET", "/emails/fetch") => Some(fetch_start(state)),
        ("GET", "/emails/fetch/status") => Some(fetch_status(state, true)),
        ("GET", "/proxy/image") => Some(proxy_image(req)),
        ("GET", "/contacts/suggest") => Some(contacts_suggest(state, req)),
        // M3-3 联系人管理页：整页 + 删除（htmx 片段刷新列表）
        ("GET", "/contacts") => Some(contacts_page(state, req)),
        ("POST", "/contacts/delete") => Some(contacts_delete(state, req)),
        // B3 手动新增/编辑联系人（addr 主键不可改；已存在时只更新姓名）
        ("POST", "/contacts/save") => Some(contacts_save(state, req)),
        ("GET", "/side/frame") => Some(side_frame(state, req)),
        // A4 全部另存：必须在通用 attachments 分支之前（all 不是数字 id）
        ("POST", p) if p.starts_with("/emails/") && p.ends_with("/attachments/all") => {
            Some(attachments_save_all(state, req))
        }
        ("POST", p) if p.starts_with("/emails/") && p.contains("/attachments/") => {
            Some(attachment_save(state, req))
        }
        // B8 Office 预览：安全清洗后的字节与纯文本兜底。
        // 必须在通用 /view 分支之前（同为多段 GET 路径）
        ("GET", p)
            if p.starts_with("/emails/")
                && p.contains("/attachments/")
                && p.ends_with("/office-text") =>
        {
            Some(attachment_office_text(state, req))
        }
        ("GET", p)
            if p.starts_with("/emails/")
                && p.contains("/attachments/")
                && p.ends_with("/office") =>
        {
            Some(attachment_office(state, req))
        }
        ("GET", p)
            if p.starts_with("/emails/") && p.contains("/attachments/") && p.ends_with("/view") =>
        {
            Some(attachment_view(state, req))
        }
        // M1-6 发送状态轮询（/emails/{id}/status）：须在 email_detail_page
        // 通配（仅匹配纯数字 id 路径）之前注册——虽然通配不会吃掉多段路径，
        // 显式保留顺序避免将来通配调整后行为漂移
        ("GET", p) if p.starts_with("/emails/") && p.ends_with("/status") => {
            Some(email_status(state, req))
        }
        ("GET", p) if p.starts_with("/emails/") => email_detail_page(state, req),
        // M2-2 会话详情（GET /threads/{id}）：按组内任一邮件 id 定位 thread_key 串列
        ("GET", p) if p.starts_with("/threads/") => thread_detail_page(state, req),
        ("GET", "/welcome") => Some(render_html("welcome.html", &())),
        ("GET", "/welcome/setup") => Some(wizard_setup_page(state)),
        ("POST", "/welcome/setup/save") => Some(wizard_save(state, req, tester)),
        ("GET", "/welcome/done") => Some(wizard_done(state)),
        ("GET", "/settings") => Some(settings_page(state, "account")),
        ("GET", "/settings/account")
        | ("GET", "/settings/sync")
        | ("GET", "/settings/storage")
        | ("GET", "/settings/templates")
        | ("GET", "/settings/about") => {
            let tab = req.path.rsplit('/').next().unwrap_or("account");
            Some(settings_page(state, tab))
        }
        ("GET", p) if p.starts_with("/settings/panes/") => {
            let tab = p.strip_prefix("/settings/panes/").unwrap_or("");
            valid_tab(tab).then(|| settings_pane_fragment(state, tab))
        }
        // B2 邮件模板：保存/删除后回设置页模板片段（htmx 局部刷新）
        ("POST", "/templates/save") => Some(template_save(state, req)),
        ("POST", "/templates/delete") => Some(template_delete(state, req)),
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

/// 邮件模板保存（B2，POST /templates/save）：id 空 = 新增，否则覆盖。
/// 返回模板 pane 片段（成功 saved 提示 / 失败 error 回显），不整页跳转。
fn template_save(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let id = form
        .get("id")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0);
    let name = trimmed_field(&form, "name").unwrap_or_default();
    // 完全套用邮件正文逻辑：html 模式按 body_html 生成纯文本版（列表预览/撰写页
    // 纯文本插入回退），plain 模式直接用 body_text；不再把两个字段各自独立提交
    let body_mode = form.get("body_mode").map(String::as_str).unwrap_or("plain");
    let (body_text, body_html) = if body_mode == "html" {
        let raw = form.get("body_html").cloned().unwrap_or_default();
        (mail::strip_html(&raw), raw)
    } else {
        (
            form.get("body_text").cloned().unwrap_or_default(),
            String::new(),
        )
    };
    let (saved, error) = match mail::templates::save(state, id, &name, &body_text, &body_html) {
        Ok(_) => (true, String::new()),
        Err(e) => (false, e),
    };
    render_html(
        "settings_pane_templates.html",
        &settings_ctx(state, "templates", saved, &error, None, None),
    )
}

/// 邮件模板删除（B2，POST /templates/delete）：幂等，删除后回 pane 片段
fn template_delete(state: &AppState, req: &Req) -> Resp {
    let form = parse_form(&req.body);
    let id = form
        .get("id")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let (saved, error) = match mail::templates::delete(state, id) {
        Ok(()) => (true, String::new()),
        Err(e) => (false, e),
    };
    render_html(
        "settings_pane_templates.html",
        &settings_ctx(state, "templates", saved, &error, None, None),
    )
}

fn valid_tab(tab: &str) -> bool {
    matches!(tab, "account" | "sync" | "storage" | "templates" | "about")
}

// ---------- 页面 ----------

fn folder_label(folder: &str) -> &'static str {
    match folder {
        "inbox" => "收件箱",
        "sent" => "发件箱",
        "draft" => "草稿箱",
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
    let page = clamp_page(req.query_param("page").as_deref());
    let unread_only = req.query_param("unread").map(|v| v == "1").unwrap_or(false);
    let external_only = req
        .query_param("external")
        .map(|v| v == "1")
        .unwrap_or(false);
    let q_input = req.query_param("q").unwrap_or_default().trim().to_string();
    // 每页条数：URL per 优先（选中即跳转并顺手持久化到 settings，下次无参
    // 访问沿用；非法/缺省回退 settings，再退默认 10）。非默认值进 base
    // /list_query，翻页、筛选开关、详情往返全部保持用户选择
    let per_url = clamp_per(req.query_param("per").as_deref());
    let settings_per = per_from_settings(state);
    let per = per_url.unwrap_or(settings_per);
    if let Some(p) = per_url
        && p != settings_per
    {
        let _ = state.set_batch(&[(keys::PAGE_SIZE, p.to_string().as_str())]);
    }
    // 内部域取账号域名（默认 chinatelecom.cn）：判定外部发件人的唯一口径
    let d = defaults();
    let internal_domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    // 排序（A2）：URL sort 解析，非默认值进 base/list_query；默认不携带保持 URL 干净
    let sort = mail::SortKey::parse(req.query_param("sort").as_deref());
    // 高级检索（B1）：发件人包含 / 日期区间（YYYY-MM-DD，结束日含当天 23:59:59）/
    // 仅看附件。非法日期静默忽略（不回错误页，按未填处理）
    let from_filter = req
        .query_param("from")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let df_raw = req.query_param("df").unwrap_or_default();
    let dt_raw = req.query_param("dt").unwrap_or_default();
    let date_from = mail::date_epoch_of(&df_raw, false).unwrap_or(0);
    let date_to = mail::date_epoch_of(&dt_raw, true).unwrap_or(0);
    let has_attach = req.query_param("att").map(|v| v == "1").unwrap_or(false);
    let q = mail::ListQuery {
        folder: folder.clone(),
        page,
        unread_only,
        external_only,
        internal_domain,
        per,
        sort,
        from_filter: from_filter.clone(),
        date_from,
        date_to,
        has_attach,
    };
    // 会话视图（M2-2）：mode=thread 且仅收件箱/全部下可用（草稿/已发保持单封）；
    // 搜索下同样可切会话（命中代表行=最新一封）
    let threadable = matches!(folder.as_str(), "inbox" | "all");
    let is_thread = threadable
        && req
            .query_param("mode")
            .map(|v| v == "thread")
            .unwrap_or(false);
    // 全文检索（§4.3）：有有效词走 FTS+BM25；空输入/全停用词/检索异常回退普通列表。
    // 会话模式按 thread_key 归组取代表行（list_threads），单封语义不变
    let (items, threads, total) = if is_thread {
        let (threads, total) = mail::list_threads(state, &q).unwrap_or_default();
        (Vec::new(), threads, total)
    } else {
        let (items, total) = if q_input.is_empty() {
            mail::list(state, &q).unwrap_or_default()
        } else {
            match mail::search(state, &q_input, &q) {
                Ok(Some(res)) => res,
                Ok(None) | Err(_) => mail::list(state, &q).unwrap_or_default(),
            }
        };
        (items, Vec::new(), total)
    };
    let pages = (total + per - 1) / per;
    // 基址 = folder + q + per（不含筛选）：两个开关各自"取反自己、保留对方"，
    // 否则开关一次会丢掉另一个筛选（多筛选项并存时的常见坑）；per 恒带
    // （默认 10 除外，保持 URL 干净）
    let mut base = if folder == "inbox" {
        "/?".to_string()
    } else {
        format!("/?folder={folder}&")
    };
    if !q_input.is_empty() {
        base = format!("{base}q={}&", urlencode(&q_input));
    }
    if per != PER_DEFAULT {
        base = format!("{base}per={per}&");
    }
    if sort != mail::SortKey::DateDesc {
        base = format!("{base}sort={}&", sort.as_str());
    }
    // 高级检索条件：翻页/筛选开关/会话切换全部保持（日期只在解析成功时携带）
    if !from_filter.is_empty() {
        base = format!("{base}from={}&", urlencode(&from_filter));
    }
    if date_from > 0 {
        base = format!("{base}df={}&", urlencode(&df_raw));
    }
    if date_to > 0 {
        base = format!("{base}dt={}&", urlencode(&dt_raw));
    }
    if has_attach {
        base = format!("{base}att=1&");
    }
    let unread_part = if unread_only { "unread=1&" } else { "" };
    let external_part = if external_only { "external=1&" } else { "" };
    let page_base = format!("{base}{unread_part}{external_part}");
    // 筛选开关需保持当前视图（会话/单封），否则在会话视图点筛选会退回单封
    let view_keep = if is_thread { "mode=thread&" } else { "" };
    let unread_href = if unread_only {
        // 取消未读筛选：保留外部筛选与当前视图，回到当前页
        format!("{base}{view_keep}{external_part}page={page}")
    } else {
        format!("{base}{view_keep}unread=1&{external_part}page=1")
    };
    let external_href = if external_only {
        format!("{base}{view_keep}{unread_part}page={page}")
    } else {
        format!("{base}{view_keep}external=1&{unread_part}page=1")
    };
    // 会话/单封视图切换链接：保留 folder/unread/external/q，翻页重置（两种视图行数不同）
    // 目标 = 当前态取反 mode；仅 threadable（收件箱/全部）时渲染切换按钮
    let mode_part = if is_thread { "" } else { "mode=thread&" };
    let thread_href = format!("{base}{mode_part}{unread_part}{external_part}page=1");
    // 列表完整查询串（详情往返携带；folder=inbox 且无筛选时为空 → 链接保持简洁）
    let mut list_query = String::new();
    if folder != "inbox" {
        list_query.push_str(&format!("folder={folder}"));
    }
    // 会话视图往返：详情页「返回列表」必须回到会话列表而非单封视图
    if is_thread {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str("mode=thread");
    }
    if unread_only {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str("unread=1");
    }
    if external_only {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str("external=1");
    }
    if !q_input.is_empty() {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("q={}", urlencode(&q_input)));
    }
    if per != PER_MIN {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("per={per}"));
    }
    if sort != mail::SortKey::DateDesc {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("sort={}", sort.as_str()));
    }
    if !from_filter.is_empty() {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("from={}", urlencode(&from_filter)));
    }
    if date_from > 0 {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("df={}", urlencode(&df_raw)));
    }
    if date_to > 0 {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("dt={}", urlencode(&dt_raw)));
    }
    if has_attach {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str("att=1");
    }
    if page > 1 {
        if !list_query.is_empty() {
            list_query.push('&');
        }
        list_query.push_str(&format!("page={page}"));
    }
    let ctx = MainCtx {
        frame: frame_ctx(state, &folder),
        folder: folder.clone(),
        folder_label: folder_label(&folder).into(),
        unread_only,
        external_only,
        unread_href,
        external_href,
        list_query,
        page,
        pages,
        total,
        per,
        page_base,
        q: q_input,
        sort: sort.as_str().to_string(),
        from_filter,
        df: if date_from > 0 { df_raw } else { String::new() },
        dt: if date_to > 0 { dt_raw } else { String::new() },
        has_attach,
        items,
        is_thread,
        threads,
        thread_href,
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
    let d = defaults();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let from_external = mail::is_external_addr(&email.from_addr, &domain);
    let to_chips = addr_chips(&email.to_addrs, &domain);
    let cc_chips = addr_chips(&email.cc_addrs, &domain);
    let bcc_chips = addr_chips(&email.bcc_addrs, &domain);
    let back_href = detail_back_href(req, &folder);
    let list_query = list_query_of(req, &folder);
    let export_name = mail::eml::suggested_filename(&email.subject, email.id);
    let export_md_name = mail::md::suggested_filename(&email.subject, email.id);
    Some(render_html(
        "email.html",
        &EmailCtx {
            frame: frame_ctx(state, &folder),
            email,
            back_href,
            list_query,
            from_external,
            to_chips,
            cc_chips,
            bcc_chips,
            notice,
            export_name,
            export_md_name,
        },
    ))
}

/// 源码页（GET /emails/{id}/source?part=html|text|headers，A5）。
/// 只读展示，不改任何状态；切换 tab 保留列表上下文（list_query）。
fn email_source(state: &AppState, req: &Req) -> Option<Resp> {
    let id = path_id(&req.path, "/emails/")?;
    if !state.configured() {
        return Some(render_html("welcome.html", &()));
    }
    mark_read_and_sync_seen(state, id);
    let email = mail::get(state, id)?;
    let folder = email.folder.clone();
    let part = match req.query_param("part").as_deref() {
        Some("text") => "text",
        Some("headers") => "headers",
        _ => "html",
    };
    let (content, empty_hint) = match part {
        "text" => (
            email.body_text.clone(),
            "该邮件没有纯文本正文（仅存 HTML 版式）。",
        ),
        "headers" => (
            email.raw_headers.clone(),
            "该邮件收取于旧版本，未保存原始信头；此后新收的邮件会自动保存。",
        ),
        _ => (
            email.body_html.clone(),
            "该邮件没有 HTML 版式（纯文本邮件），可查看「纯文本」标签。",
        ),
    };
    let chars = content.chars().count();
    let lines = content.lines().count();
    Some(render_html(
        "source.html",
        &SourceCtx {
            frame: frame_ctx(state, &folder),
            email_id: id,
            subject: email.subject.clone(),
            part: part.to_string(),
            content,
            chars,
            lines,
            empty_hint: empty_hint.to_string(),
            back_href: detail_back_href(req, &folder),
            list_query: list_query_of(req, &folder),
        },
    ))
}

/// 会话详情页（GET /threads/{id}，M2-2）：定位 id 所在 thread_key，串列组内往来。
/// 未配置 → 欢迎页；id 不属于任何会话组 → 404
fn thread_detail_page(state: &AppState, req: &Req) -> Option<Resp> {
    let id = path_id(&req.path, "/threads/")?;
    if !state.configured() {
        return Some(render_html("welcome.html", &()));
    }
    let d = defaults();
    let domain = state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default();
    let detail = mail::thread_mails(state, id, &domain).ok().flatten()?;
    // 所属文件夹取组内最新一封（正序末项；frame 高亮 + 返回列表）
    let folder = detail
        .items
        .last()
        .map(|m| m.folder.clone())
        .unwrap_or_else(|| "inbox".to_string());
    let back_href = if folder == "inbox" {
        "/".to_string()
    } else {
        format!("/?folder={folder}")
    };
    Some(render_html(
        "thread.html",
        &ThreadCtx {
            frame: frame_ctx(state, &folder),
            detail,
            folder,
            back_href,
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
    // 删除后回列表 / 失败回详情：均携带进入前的列表上下文（过滤+翻页）
    let back_href = detail_back_href(req, &folder);
    let list_query = list_query_of(req, &folder);
    match mail::delete(state, id) {
        Ok(_) => {
            state.poke_tray(); // 删未读邮件会减未读数：刷托盘角标
            Resp::hx_redirect(&back_href)
        }
        Err(e) => {
            let mut q = list_query;
            if !q.is_empty() {
                q.push('&');
            }
            q.push_str(&format!("error={}", urlencode(&format!("删除失败：{e}"))));
            Resp::hx_redirect(&format!("/emails/{id}?{q}"))
        }
    }
}

/// 撤销发送（POST /emails/{id}/undo-send，A1）：只在撤销窗口内有效
/// （status=draft 且 scheduled_at 未到点）。成功回撰写页继续编辑；
/// 窗口已过（已入队/已发出）回详情页提示，不误改发送状态。
fn email_undo_send(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    if mail::cancel_scheduled_send(state, id) {
        Resp::hx_redirect(&format!("/compose?draft_id={id}&undone=1"))
    } else {
        let msg = urlencode("发送已开始，撤销窗口已过（可在发件箱确认）");
        Resp::hx_redirect(&format!("/emails/{id}?error={msg}"))
    }
}

/// 导出 .eml（B4，POST /emails/{id}/export-eml）：前端保存对话框选好 dest 后调用。
/// 由库内字段重建 RFC 5322 报文（eml::build_eml），写盘失败回错误 toast。
fn email_export_eml(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    let form = parse_form(&req.body);
    let Some(raw_dest) = trimmed_field(&form, "dest") else {
        return Resp::html(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">未选择保存位置</div>"#.into(),
        );
    };
    // 保存对话框里改了文件名、忘写后缀时自动补全（带其他后缀也补，
    // 保证双击即可用邮件客户端打开）
    let dest = ensure_ext(&raw_dest, ".eml");
    let toast = |cls: &str, msg: &str| {
        Resp::html(format!(
            r#"<div id="toast" class="toast {cls}" hx-swap-oob="true">{}</div>"#,
            html_escape(msg)
        ))
    };
    if let Err(e) = mail::guard_sensitive_path(state, &dest) {
        return toast("err", &e);
    }
    match mail::eml::build_eml(state, id) {
        Ok(bytes) => match std::fs::write(&dest, bytes) {
            Ok(()) => toast("ok", &format!("已导出到 {dest}")),
            Err(e) => toast("err", &format!("写入失败：{e}")),
        },
        Err(e) => toast("err", &e),
    }
}

/// 导出 Markdown（POST /emails/{id}/export-md）：与 .eml 导出同一交互模式
/// （前端保存对话框选好 dest 后 POST），由 md::build_md 组装
/// front matter + 正文 + 附件清单的 MD 文本写盘，方便交给 AI 分析处理。
fn email_export_md(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    let form = parse_form(&req.body);
    let Some(raw_dest) = trimmed_field(&form, "dest") else {
        return Resp::html(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">未选择保存位置</div>"#.into(),
        );
    };
    let dest = ensure_ext(&raw_dest, ".md");
    let toast = |cls: &str, msg: &str| {
        Resp::html(format!(
            r#"<div id="toast" class="toast {cls}" hx-swap-oob="true">{}</div>"#,
            html_escape(msg)
        ))
    };
    if let Err(e) = mail::guard_sensitive_path(state, &dest) {
        return toast("err", &e);
    }
    match mail::md::build_md(state, id) {
        Ok(text) => match std::fs::write(&dest, text) {
            Ok(()) => toast("ok", &format!("已导出到 {dest}")),
            Err(e) => toast("err", &format!("写入失败：{e}")),
        },
        Err(e) => toast("err", &e),
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

/// 标为未读（POST /emails/{id}/unread，A5 配套）：详情页主动留待办。
/// 只改本地 unread（不回写 IMAP \Seen，服务器无"反向已读"语义），
/// 完成后跳回列表并保持过滤/页码上下文。
fn email_unread(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    let folder = mail::get(state, id)
        .map(|d| d.folder)
        .unwrap_or_else(|| "inbox".into());
    mail::mark_unread(state, id);
    state.poke_tray(); // 未读数 +1：刷托盘角标
    Resp::hx_redirect(&detail_back_href(req, &folder))
}

/// 星标翻转（POST /emails/{id}/star，M2-1）。返回空体 + mbx:starDone 事件
/// 携带翻转后的星标态：列表页据此重拉列表片段（星标置顶会重排行序），
/// 详情页就地翻转按钮态（无重排）。 hx-swap="none" 由按钮侧声明
fn email_star(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    match mail::toggle_star(state, id) {
        Some(starred) => Resp::html(String::new()).with_event_detail(
            "mbx:starDone",
            &serde_json::json!({ "starred": starred }).to_string(),
        ),
        None => Resp::not_found(),
    }
}

/// 本地置已读（从未读翻转时同时写 seen_pending 并唤醒回写 worker）。
///
/// 旧实现每次点击 spawn 一个线程做完整 IMAP 登录，连点 N 封 = N 次登录（企业服务器
/// 易限流）。现在本地操作只落库 + 唤醒；worker 在 1.5s 合并窗口后单次连接批量回写
fn mark_read_and_sync_seen(state: &AppState, id: i64) {
    mail::mark_read(state, id);
    state.poke_tray(); // 本地未读数变化：刷托盘角标（M4-1；调度线程兑底兑漏）
}

/// 批量操作（POST /emails/batch，M1-1）双模式：
/// - 页内精确：`ids`（JSON 数组）
/// - 跨页全选：`select_all=1` + `exclude_ids`（JSON 数组，可空）+
///   `folder/unread/external/q`（与列表 URL 同名参数，走同一白名单）
///
/// action：read|unread|delete。返回 mail_list 片段（整段换入，页码可能收缩）
/// + mbx:fetchDone（边栏计数/未读角标刷新）。
fn emails_batch(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let form = parse_form(&req.body);
    let Some(action) =
        mail::BatchAction::parse(form.get("action").map(String::as_str).unwrap_or(""))
    else {
        return batch_error_fragment("未知操作");
    };
    // 目标解析：跨页全选优先；否则 ids（两者都缺 → 无目标）
    let target = if form.get("select_all").map(|v| v == "1").unwrap_or(false) {
        let d = defaults();
        let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
        let q = mail::ListQuery {
            folder: form
                .get("folder")
                .cloned()
                .unwrap_or_else(|| "inbox".into()),
            page: 1, // 目标解析与页无关：全量命中（per 同理不影响结果集）
            unread_only: form.get("unread").map(|v| v == "1").unwrap_or(false),
            external_only: form.get("external").map(|v| v == "1").unwrap_or(false),
            internal_domain: g(keys::ACCOUNT_DOMAIN),
            // 不被 SelectAll 目标解析消费（batch_apply 只用过滤条件），取默认值
            // 防未来误用成 LIMIT 1
            per: PER_DEFAULT,
            // 排序不影响命中集（只影响 ORDER BY），按表单值回填即可
            sort: mail::SortKey::parse(form.get("sort").map(String::as_str)),
            // 高级检索（B1）：与列表同口径；日期非法按不限处理
            from_filter: form
                .get("from")
                .map(|v| v.trim().to_lowercase())
                .unwrap_or_default(),
            date_from: form
                .get("df")
                .and_then(|v| mail::date_epoch_of(v, false))
                .unwrap_or(0),
            date_to: form
                .get("dt")
                .and_then(|v| mail::date_epoch_of(v, true))
                .unwrap_or(0),
            has_attach: form.get("att").map(|v| v == "1").unwrap_or(false),
        };
        // q（检索词）仅在非空时参与：批量检索结果全选 = 命中即选中。
        // 检索走 FTS MATCH，与列表检索同源；此处简化为非空时拒绝批量（检索
        // 词场景跨页全选与列表高亮口径不一致的风险高，先用页内 ids 模式覆盖）
        let has_search = form.get("q").map(|v| !v.trim().is_empty()).unwrap_or(false)
            || form
                .get("from")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            || form
                .get("df")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            || form
                .get("dt")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            || form.get("att").map(|v| v == "1").unwrap_or(false);
        if has_search {
            return batch_error_fragment("检索/高级筛选结果下暂不支持跨页全选，请使用页内选择");
        }
        mail::BatchTarget::SelectAll {
            exclude: form_json_i64s(&form, "exclude_ids"),
            query: q,
        }
    } else {
        mail::BatchTarget::Ids(form_json_i64s(&form, "ids"))
    };
    // 执行计数：delete 需确认——由前端 hx-confirm 承担（数量动态，见 app.js）
    let affected = match mail::batch_apply(state, &target, action) {
        Ok(n) => n,
        Err(e) => return batch_error_fragment(&e),
    };
    if affected == 0 {
        return batch_error_fragment("未选中任何邮件");
    }
    let _ = affected;
    // 回列表片段：不带 select 上下文（操作后选择清零），保持当前 folder/筛选；
    // 删除可能收缩页数 → 页码 clamp 由 list_root 承担（这里直接复用其解析）
    let list_req = reconstruct_list_req(req, &form);
    let resp = list_root(state, &list_req);
    state.poke_tray(); // 标已读/未读/删除改变未读数：刷托盘角标（M4-1）
    // 改变计数的动作（delete/已读类影响未读角标）附带边栏刷新事件
    resp.with_event("mbx:fetchDone")
}

/// 批量操作的错误反馈：回当前列表片段 + 顶部错误提示（复用 error 通道，
/// 与删除失败同交互形态）。简化实现：错误文案以 toast 形态返回给 #toast 容器。
fn batch_error_fragment(msg: &str) -> Resp {
    Resp::html(format!(
        r#"<div id="toast" class="toast err" hx-swap-oob="true">{}</div>"#,
        html_escape(msg)
    ))
}

/// 从批量 POST 表单重建 GET 列表请求（list_root 需要 Req 形态）：
/// folder/unread/external/q 与 page 从表单取（前端提交时携带当前列表状态）。
fn reconstruct_list_req(req: &Req, form: &HashMap<String, String>) -> Req {
    let mut qs: Vec<String> = Vec::new();
    let folder = form.get("folder").map(String::as_str).unwrap_or("inbox");
    if folder != "inbox" {
        qs.push(format!("folder={folder}"));
    }
    if form.get("unread").map(|v| v == "1").unwrap_or(false) {
        qs.push("unread=1".into());
    }
    if form.get("external").map(|v| v == "1").unwrap_or(false) {
        qs.push("external=1".into());
    }
    if let Some(q) = form.get("q").filter(|v| !v.trim().is_empty()) {
        qs.push(format!("q={}", urlencode(q)));
    }
    // 每页条数与排序：批量后重渲列表片段需保持用户选择（否则回退默认视感突兀）
    if let Some(p) = form.get("per").filter(|v| v.parse::<i64>().is_ok()) {
        qs.push(format!("per={p}"));
    }
    if let Some(s) = form
        .get("sort")
        .filter(|v| mail::SortKey::parse(Some(v)) != mail::SortKey::DateDesc)
    {
        qs.push(format!("sort={}", urlencode(s)));
    }
    // 高级检索（B1）：批量后重渲列表保持筛选条件
    if let Some(f) = form.get("from").map(|v| v.trim()).filter(|v| !v.is_empty()) {
        qs.push(format!("from={}", urlencode(f)));
    }
    if let Some(d) = form
        .get("df")
        .filter(|v| mail::date_epoch_of(v, false).is_some())
    {
        qs.push(format!("df={}", urlencode(d)));
    }
    if let Some(d) = form
        .get("dt")
        .filter(|v| mail::date_epoch_of(v, true).is_some())
    {
        qs.push(format!("dt={}", urlencode(d)));
    }
    if form.get("att").map(|v| v == "1").unwrap_or(false) {
        qs.push("att=1".into());
    }
    if let Some(p) = form.get("page").filter(|v| v.parse::<i64>().is_ok()) {
        qs.push(format!("page={p}"));
    }
    Req {
        method: "GET".into(),
        path: req.path.clone(),
        query: qs.join("&"),
        is_htmx_request: true,
        body: Vec::new(),
    }
}

/// 联系人建议（GET /contacts/suggest?q=）：撰写页收/抄送输入框模糊匹配。
/// 返回 JSON 数组 [{name, addr}]，最多 8 条；未配置返回空数组。
fn contacts_suggest(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return Resp::json("[]".into());
    }
    let q = req.query_param("q").unwrap_or_default();
    let list = state
        .with_conn(|conn| mail::contacts::suggest(conn, &q, 8))
        .unwrap_or_default();
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|c| serde_json::json!({ "name": c.name, "addr": c.addr }))
        .collect();
    Resp::json(serde_json::to_string(&items).unwrap_or_else(|_| "[]".into()))
}

/// 联系人管理页（M3-3，GET /contacts）：搜索 + 全量列表（上限 500 条，
/// 桌面办公联系人量级足够），每条含最近联系时间/使用次数/删除。
/// htmx 片段（删除后刷新）与整页同构渲染 contacts_area.html
#[derive(Serialize)]
struct ContactsCtx {
    frame: FrameCtx,
    q: String,
    total: usize,
    items: Vec<ContactView>,
    /// 每页条数（当前生效值：URL/表单 per 或 settings，默认 10）
    per: i64,
    /// 当前页（1 起；越界钳回有效区间）
    page: i64,
    /// 总页数（>=1）
    pages: i64,
    /// 翻页链接前缀（含 per/q/sort；模板拼 page=N）
    page_base: String,
    /// 排序键回显（recent/hits/name；模板选中态比较）
    sort: String,
    /// 手动保存结果（B3；空串 = 无提示）
    notice: String,
    error: String,
    /// 编辑态（B3）：地址非空时顶部表单预填并锁定地址
    edit_addr: String,
    edit_name: String,
    editing: bool,
}

/// 联系人条目展示（首字母头像 + 最近联系时间标签）
#[derive(Serialize)]
struct ContactView {
    addr: String,
    name: String,
    /// 头像字符（姓名/地址首字符，大写）
    avatar: String,
    time_label: String,
    hits: i64,
    /// 编辑链接的地址参数（已百分号编码；模板直接拼 query）
    edit_qs: String,
}

/// 联系人页当前列表状态（搜索/分页/排序；URL 态与 POST 保存/删除带回共用）：
/// 字段收敛为结构体，避免渲染函数参数膨胀
struct ContactListOpts<'a> {
    q: &'a str,
    per: i64,
    page: i64,
    sort: &'a str,
}

/// 联系人列表渲染（整页 contacts.html 与删除/保存后片段 contacts_area.html 共用）
fn contacts_ctx(state: &AppState, opts: &ContactListOpts<'_>) -> ContactsCtx {
    contacts_ctx_full(state, opts, "", "", "")
}

fn contacts_ctx_with(
    state: &AppState,
    opts: &ContactListOpts<'_>,
    notice: &str,
    error: &str,
) -> ContactsCtx {
    contacts_ctx_full(state, opts, "", notice, error)
}

/// 联系人分页解析：per 与 mail list 同口径（URL/表单 per 优先，缺省用 settings，
/// 非默认值顺手持久化到 PAGE_SIZE）；page 越界钳回有效区间
fn contact_paging(state: &AppState, per_raw: Option<&str>, page_raw: Option<&str>) -> (i64, i64) {
    let per_url = clamp_per(per_raw);
    let settings_per = per_from_settings(state);
    let per = per_url.unwrap_or(settings_per);
    if let Some(p) = per_url
        && p != settings_per
    {
        let _ = state.set_batch(&[(keys::PAGE_SIZE, p.to_string().as_str())]);
    }
    let page = clamp_page(page_raw);
    (per, page)
}

/// 联系人翻页链接前缀：per 恒带，q 非空追加；sort 非默认追加（与
/// mail list 的「默认不携带保持 URL 干净」同一口径）；模板拼 &page=N
fn contact_page_base(q: &str, per: i64, sort: &str) -> String {
    let mut base = format!("/contacts?per={per}");
    if !q.is_empty() {
        base.push('&');
        base.push_str(&format!("q={}", urlencode(q)));
    }
    if sort != "recent" {
        base.push_str(&format!("&sort={sort}"));
    }
    base
}

/// 编辑态取姓名（地址不存在则退化为新增表单，不报错）
fn contacts_ctx_full(
    state: &AppState,
    opts: &ContactListOpts<'_>,
    edit_addr: &str,
    notice: &str,
    error: &str,
) -> ContactsCtx {
    let sort = mail::contacts::ContactsSort::parse(Some(opts.sort));
    let rows = state
        .with_conn(|conn| mail::contacts::list_rows(conn, opts.q, 500, sort))
        .unwrap_or_default();
    let total = rows.len();
    let per = opts.per.max(1);
    let pages = ((total as i64) + per - 1) / per;
    let page = opts.page.clamp(1, pages.max(1));
    let start = ((page - 1) as usize) * (per as usize);
    let page_rows = rows
        .into_iter()
        .skip(start)
        .take(per as usize)
        .collect::<Vec<_>>();
    let items = page_rows
        .into_iter()
        .map(|r| ContactView {
            addr: r.addr.clone(),
            name: r.name.clone(),
            avatar: r
                .name
                .chars()
                .next()
                .or(r.addr.chars().next())
                .unwrap_or('?')
                .to_uppercase()
                .to_string(),
            time_label: if r.updated_at > 0 {
                mail::time_label(r.updated_at)
            } else {
                "—".into()
            },
            edit_qs: urlencode(&r.addr),
            hits: r.hits,
        })
        .collect::<Vec<_>>();
    ContactsCtx {
        frame: frame_ctx(state, "contacts"),
        q: opts.q.to_string(),
        total,
        items,
        per,
        page,
        pages,
        page_base: contact_page_base(opts.q, per, sort.key()),
        sort: sort.key().to_string(),
        notice: notice.to_string(),
        error: error.to_string(),
        edit_name: if edit_addr.is_empty() {
            String::new()
        } else {
            state
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT name FROM contacts WHERE addr = ?1",
                        [edit_addr],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                })
                .ok()
                .flatten()
                .unwrap_or_default()
        },
        edit_addr: edit_addr.to_string(),
        editing: !edit_addr.is_empty(),
    }
}

fn contacts_page(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let q = req.query_param("q").unwrap_or_default().trim().to_string();
    let edit = req.query_param("edit").unwrap_or_default();
    let (per, page) = contact_paging(
        state,
        req.query_param("per").as_deref(),
        req.query_param("page").as_deref(),
    );
    let sort = req.query_param("sort").unwrap_or_default();
    let opts = ContactListOpts {
        q: &q,
        per,
        page,
        sort: sort.trim(),
    };
    let ctx = contacts_ctx_full(state, &opts, edit.trim(), "", "");
    if req.is_htmx_request {
        render_html("contacts_area.html", &ctx)
    } else {
        render_html("contacts.html", &ctx)
    }
}

/// 新增/编辑联系人（B3，POST /contacts/save）：返回当前搜索态列表片段。
/// addr 是主键，编辑只改姓名；校验失败/超长回错误提示，不落库。
fn contacts_save(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let form = parse_form(&req.body);
    let addr_raw = form.get("addr").cloned().unwrap_or_default();
    let name = form.get("name").cloned().unwrap_or_default();
    let q = form
        .get("q")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let (notice, error) = match mail::contacts::validate_addr(&addr_raw) {
        Ok(_addr) if name.trim().chars().count() > 60 => {
            (String::new(), "姓名最多 60 个字符".to_string())
        }
        Ok(addr) => {
            let now = mail::now_epoch();
            match state.with_conn(|conn| mail::contacts::save_manual(conn, &addr, &name, now)) {
                Ok(()) => (format!("已保存联系人 {addr}"), String::new()),
                Err(e) => (String::new(), format!("保存失败：{e}")),
            }
        }
        Err(e) => (String::new(), e),
    };
    let (per, page) = contact_paging(
        state,
        form.get("per").map(|s| s.as_str()),
        form.get("page").map(|s| s.as_str()),
    );
    let sort = form.get("sort").cloned().unwrap_or_default();
    let opts = ContactListOpts {
        q: &q,
        per,
        page,
        sort: sort.trim(),
    };
    let ctx = contacts_ctx_with(state, &opts, &notice, &error);
    render_html("contacts_area.html", &ctx)
}

/// 删除联系人（M3-3，POST /contacts/delete）：删后返回当前搜索态列表片段
/// （htmx target=#contacts-area 换入；hx-include 搜索框保过滤）。
/// 下次与该地址往来时自动采集会重新建档（预期行为，无需提示）。
fn contacts_delete(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let form = parse_form(&req.body);
    let addr = form
        .get("addr")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !addr.is_empty() {
        let _ = state.with_conn(|conn| mail::contacts::remove(conn, &addr));
    }
    // 保留搜索词：删除后回到同一过滤视图
    let q = form
        .get("q")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let (per, page) = contact_paging(
        state,
        form.get("per").map(|s| s.as_str()),
        form.get("page").map(|s| s.as_str()),
    );
    let sort = form.get("sort").cloned().unwrap_or_default();
    let opts = ContactListOpts {
        q: &q,
        per,
        page,
        sort: sort.trim(),
    };
    let ctx = contacts_ctx(state, &opts);
    render_html("contacts_area.html", &ctx)
}

/// 手动收取（GET /emails/fetch，§6.2）：启动一轮（已在收件中则忽略）后回进行中片段
/// 邮件外部图片代理（GET /proxy/image?url=…）。
///
/// HTML 正文内的远程图片统一经本机拉取后回传，WebView 不直接向第三方发请求，
/// 避免暴露 Referer/客户端指纹；同时阻断内网地址（SSRF）。只回传 image/*，
/// 限制大小与超时，且不跟随重定向（防跳转到内网）。
fn proxy_image(req: &Req) -> Resp {
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let Some(raw) = req.query_param("url") else {
        return proxy_error(400, "缺少 url");
    };
    let Ok(parsed) = url::Url::parse(&raw) else {
        return proxy_error(400, "URL 无效");
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return proxy_error(400, "仅支持 http/https");
    }
    // DNS 重绑定加固：先解析一次并校验全部地址为公网，再把这批地址固定给
    // resolver 建连——连接不再二次解析，恶意 DNS 无法在校验与建连之间切换
    // 回内网地址（旧实现"校验后按域名再连"的残留风险，至此闭合）。
    let addrs = match resolve_public_addrs(&parsed) {
        Ok(a) => a,
        Err(msg) => return proxy_error(403, msg),
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .redirects(0)
        .resolver(move |_: &str| Ok(addrs.clone()))
        .build();
    let resp = match agent.get(&raw).set("Accept", "image/*").call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return proxy_error(502, &format!("上游返回 {code}")),
        Err(_) => return proxy_error(502, "图片拉取失败"),
    };
    let ctype = resp.header("Content-Type").unwrap_or("").to_string();
    if !ctype.starts_with("image/") {
        return proxy_error(400, "目标不是图片");
    }
    let mut body = Vec::new();
    if resp
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return proxy_error(502, "图片读取失败");
    }
    if body.len() as u64 > MAX_BYTES {
        return proxy_error(400, "图片过大");
    }
    Resp {
        status: 200,
        content_type: ctype,
        cache_control: "private, max-age=3600",
        body,
        extra_headers: Vec::new(),
    }
}

fn proxy_error(status: u16, msg: &str) -> Resp {
    Resp {
        status,
        content_type: "text/plain; charset=utf-8".into(),
        cache_control: "no-store",
        body: msg.as_bytes().to_vec(),
        extra_headers: Vec::new(),
    }
}

/// 解析 URL 主机并校验全部为公网地址，返回固定给 resolver 的地址集。
/// 字面量 IP 直接构造（不经过系统解析器）；域名解析一次（localhost 与
/// 无点单标签拒绝，与旧口径一致）。任一地址命中内网即整体拒绝。
fn resolve_public_addrs(parsed: &url::Url) -> Result<Vec<SocketAddr>, &'static str> {
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = match parsed.host() {
        Some(url::Host::Domain(d)) => {
            if d.eq_ignore_ascii_case("localhost") || !d.contains('.') {
                return Err("禁止访问内网地址");
            }
            (d, port)
                .to_socket_addrs()
                .map_err(|_| "无法解析服务器地址")?
                .collect()
        }
        Some(url::Host::Ipv4(v)) => vec![SocketAddr::from((v, port))],
        Some(url::Host::Ipv6(v)) => vec![SocketAddr::from((v, port))],
        None => return Err("缺少主机"),
    };
    if addrs.is_empty() {
        return Err("无法解析服务器地址");
    }
    if addrs.iter().any(|a| is_private_ip(a.ip())) {
        return Err("禁止访问内网地址");
    }
    Ok(addrs)
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.octets()[0] == 0
                || (v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1])) // 100.64/10
        }
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || v.is_unique_local()
                || v.is_unicast_link_local()
        }
    }
}

fn fetch_start(state: &AppState) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    // 后台更新进行中：页面重复触发直接忽略（不排队、不叠加），仅回当前状态；
    // start_fetch 内部 CAS 亦兜底（多请求竞态时仅首个生效）
    if !state.fetch_snapshot().running {
        state.start_fetch();
    }
    fetch_status(state, false)
}

/// 收件状态片段（GET /emails/fetch/status 轮询）：进行中每 2s 自轮询；
/// 刚完成时消费标记并回 HX-Refresh 整页刷新（列表/计数全新）
/// 收件剩余时间预估（需求：预估超过 1 分钟才显示）。
///
/// 口径：首个进度上报起表（progress_started_at），按已入库增量 / 实际耗时
/// 得吞吐，外推剩余封数所需时间。只看吞吐不引入附件大小加权——批量入库
/// 的耗时与大小弱相关（每批一次事务，小件主导），复杂模型徒增跳变。
///
/// 展示门槛（三项全满足才出文案，逐项防误导）：
/// - running 且 total > done（有可估的剩余量）
/// - 计时跨度 ≥ 3s：首拍下载中吞吐严重偏低（连接+首批编码开销），
///   此时外推会把 10 秒的工作估成几分钟，跳变后再缩小观感极差
/// - 预估剩余 > 60s：短于 1 分钟的任务不显示（需求），秒级噪音不打扰
fn eta_label(s: &state::FetchState) -> String {
    if !s.running || s.fetch_total <= s.fetch_done || s.progress_started_at == 0 {
        return String::new();
    }
    let now = s.progress_updated_at.max(mail::now_epoch());
    let elapsed = (now - s.progress_started_at).max(0);
    if elapsed < 3 {
        return String::new();
    }
    let done_delta = s.fetch_done.saturating_sub(s.progress_started_done);
    if done_delta == 0 {
        // 计时内零进展（拉取挂起/大批解析中）：吞吐不可算，不估
        return String::new();
    }
    let remaining = s.fetch_total - s.fetch_done;
    let secs = (remaining as f64 / done_delta as f64 * elapsed as f64).round() as i64;
    if secs <= 60 {
        return String::new();
    }
    if secs < 90 {
        return "约 1 分钟".into();
    }
    let mins = (secs as f64 / 60.0).round() as i64;
    if mins < 60 {
        return format!("约 {mins} 分钟");
    }
    let hours = mins / 60;
    let rem = mins % 60;
    if rem == 0 || hours >= 10 {
        return format!("约 {hours} 小时");
    }
    format!("约 {hours} 小时 {rem} 分钟")
}

fn fetch_status(state: &AppState, consume: bool) -> Resp {
    if !state.configured() {
        // 未配置：状态条留空（.fetch-status:empty 隐藏），避免欢迎页出现"没有新邮件"
        return Resp::html(
            r#"<div id="fetch-status" class="fetch-status fetch-status-side"></div>"#.into(),
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
        eta_label: eta_label(&s),
        just_finished: s.just_finished,
        phase: s.phase.clone(),
    };
    let mut resp = render_html("fetch_status.html", &ctx);
    if consume && s.just_finished {
        state.clear_fetch_finish();
        // 完成通知改为 HX-Trigger 事件：app.js 收到后按当前页面做局部刷新。
        // 不再下发 HX-Refresh（location.reload）——Tauri 自定义协议下整页
        // reload 行为不稳定，且会丢失正在撰写的草稿与阅读位置。
        resp.extra_headers
            .push(("HX-Trigger".into(), "{\"mbx:fetchDone\":true}".into()));
    }
    resp
}

/// 侧栏片段（GET /side/frame?folder=）：收取完成局部刷新计数/高亮用，
/// 渲染 side.html 单片段（无 base 布局），可直接 outerHTML 换入 aside.side
fn side_frame(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return Resp::html(r#"<aside class="side"></aside>"#.into());
    }
    let folder = match req.query_param("folder").as_deref() {
        Some("all") | Some("sent") | Some("draft") | Some("inbox") => {
            req.query_param("folder").unwrap()
        }
        _ => "inbox".to_string(),
    };
    // side.html 经 include 共用，字段都挂在 frame.* 下——独立渲染须同名包一层
    render_html(
        "side.html",
        &SideFrameCtx {
            frame: frame_ctx(state, &folder),
        },
    )
}

/// side.html 独立渲染包装（对齐 include 时的变量名 frame）
#[derive(Serialize)]
struct SideFrameCtx {
    frame: FrameCtx,
}

/// 回复引用的纯文本（原邮件无 HTML 时）：逐行加「· 」前缀
fn quoted_plain(src: &mail::EmailDetail) -> String {
    let quoted = src
        .body_text
        .lines()
        .map(|l| format!("· {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\n----- 原文 -----\n{quoted}\n")
}

/// 回复引用（原邮件有 HTML 时）：引用头 + 原 HTML 裸拼接（不加 blockquote）。
/// 类型保持不变：HTML 原件 → 富文本模式回复，纯文本原件 → 纯文本回复。
/// 不用 blockquote 包裹：wangEditor 的 blockquote 节点不接纳表格（会打平为文字），
/// 配案清单等企业邮件的表格保真优先（实测 2026-11-03）。
fn quoted_html(src: &mail::EmailDetail) -> String {
    format!(
        "<p><br></p><p>----- 原文 -----</p>{}",
        mail::unwrap_top_div(&src.body_html)
    )
}

/// 预填回复正文（同时给出纯文本与 HTML 两种形态，按原邮件类型选模式）
fn fill_reply_body(ctx: &mut ComposeCtx, src: &mail::EmailDetail) {
    if src.body_html.trim().is_empty() {
        ctx.body_mode = "plain".into();
        ctx.body_text = quoted_plain(src);
    } else {
        ctx.body_mode = "html".into();
        ctx.body_html = quoted_html(src);
        // 纯文本版仅存不显（升级不可逆，但保存时服务端会重新生成）
        ctx.body_text = String::new();
    }
}

/// 撰写页（GET /compose：空表单 / ?reply_to=N 回复预填 / ?draft_id=M 草稿编辑）
fn compose_page(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let saved = req.query_param("saved").map(|v| v == "1").unwrap_or(false);
    // query_param 已解码，勿再 urldecode（二次解码会破坏含 %/+ 的错误文案）
    let error = req.query_param("error").unwrap_or_default();
    let skipped = req
        .query_param("skipped")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut ctx = ComposeCtx {
        frame: frame_ctx(state, "draft"),
        draft_id: 0,
        in_reply_to: 0,
        reply_all: false,
        to_addrs: String::new(),
        cc_addrs: String::new(),
        bcc_addrs: String::new(),
        subject: String::new(),
        body_text: String::new(),
        body_html: String::new(),
        body_mode: "plain".into(),
        attachments: Vec::new(),
        keep_ids_json: "[]".into(),
        // 草稿编辑回草稿箱，其余回来源列表（回复/转发链接携带 list_query）
        back_href: detail_back_href(
            req,
            if req.query_param("draft_id").is_some() {
                "draft"
            } else {
                "inbox"
            },
        ),
        list_query: list_query_of(
            req,
            if req.query_param("draft_id").is_some() {
                "draft"
            } else {
                "inbox"
            },
        ),
        saved,
        error,
        skipped,
        forward_from: 0,
        forward_attach_cnt: 0,
        sched_local: String::new(),
        undone: req.query_param("undone").map(|v| v == "1").unwrap_or(false),
        templates: mail::templates::list(state),
        attach_eml_from: 0,
        attach_eml_name: String::new(),
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
            ctx.bcc_addrs = draft.bcc_addrs;
            ctx.subject = draft.subject;
            ctx.body_text = draft.body_text;
            ctx.body_html = mail::unwrap_top_div(&draft.body_html);
            ctx.body_mode = if ctx.body_html.trim().is_empty() {
                "plain".into()
            } else {
                "html".into()
            };
            ctx.attachments = draft.attachments;
            // 定时发送（M4-2）：草稿的定时时刻回显到 datetime-local 控件
            if draft.scheduled_at > 0 {
                ctx.sched_local = mail::sched_local_value(draft.scheduled_at);
            }
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
        // 需求：回复时原邮件的抄送人也复制到新邮件（去自己、去重、保序）
        ctx.cc_addrs = merge_addrs(&[&src.cc_addrs], &own_address(state), &[]);
        ctx.subject = if src.subject.starts_with("Re:") {
            src.subject.clone()
        } else {
            format!("Re: {}", src.subject)
        };
        fill_reply_body(&mut ctx, &src);
    } else if let Some(rid) = req
        .query_param("reply_all_to")
        .and_then(|v| v.parse::<i64>().ok())
        && let Some(src) = mail::get(state, rid)
    {
        // 回复全部：收件人 = 原发件人 + 原收件人（去自己、去重、保序）；
        // 抄送 = 原抄送（去自己、去掉已进收件人的、去重、保序）
        ctx.in_reply_to = rid;
        ctx.reply_all = true;
        let (to, cc) = reply_all_parts(state, &src);
        ctx.to_addrs = to;
        ctx.cc_addrs = cc;
        ctx.subject = if src.subject.starts_with("Re:") {
            src.subject.clone()
        } else {
            format!("Re: {}", src.subject)
        };
        fill_reply_body(&mut ctx, &src);
    } else if let Some(eid) = req
        .query_param("edit_from")
        .and_then(|v| v.parse::<i64>().ok())
        && let Some(src) = mail::get(state, eid)
    {
        // 发件箱「重新编辑」：载入已发送邮件内容到撰写页（非转发引文）。
        // forward_from=eid 使首次保存时复制原附件，正文/主题/收件人/抄送原样回填。
        ctx.forward_from = eid;
        ctx.forward_attach_cnt = src.attach_cnt;
        ctx.to_addrs = src.to_addrs.clone();
        ctx.cc_addrs = src.cc_addrs.clone();
        ctx.bcc_addrs = src.bcc_addrs.clone();
        ctx.subject = src.subject.clone();
        if src.body_html.trim().is_empty() {
            ctx.body_mode = "plain".into();
            ctx.body_text = src.body_text.clone();
        } else {
            ctx.body_mode = "html".into();
            ctx.body_html = mail::unwrap_top_div(&src.body_html);
            ctx.body_text = String::new();
        }
    } else if let Some(eid) = req
        .query_param("attach_eml")
        .and_then(|v| v.parse::<i64>().ok())
        && let Some(src) = mail::get(state, eid)
    {
        // B6 作为附件转发：空白正文 + 原主题；.eml 在首次保存时生成附加
        ctx.attach_eml_from = eid;
        ctx.attach_eml_name = mail::eml::suggested_filename(&src.subject, eid);
        ctx.subject = if src.subject.starts_with("Fwd:") {
            src.subject.clone()
        } else {
            format!("Fwd: {}", src.subject)
        };
        ctx.body_mode = "plain".into();
        ctx.body_text = String::new();
    } else if let Some(fid) = req
        .query_param("forward_from")
        .and_then(|v| v.parse::<i64>().ok())
        && let Some(src) = mail::get(state, fid)
    {
        ctx.forward_from = fid;
        ctx.forward_attach_cnt = src.attach_cnt;
        ctx.subject = if src.subject.starts_with("Fwd:") {
            src.subject.clone()
        } else {
            format!("Fwd: {}", src.subject)
        };
        let from_disp = if src.from_name.is_empty() {
            src.from_addr.clone()
        } else {
            format!("{} <{}>", src.from_name, src.from_addr)
        };
        // 转发同样保持类型：HTML 原件 → 富文本模式；头部字段全部转义防注入
        if src.body_html.trim().is_empty() {
            let cc_line = if src.cc_addrs.trim().is_empty() {
                String::new()
            } else {
                format!("抄送：{}\n", src.cc_addrs)
            };
            ctx.body_mode = "plain".into();
            ctx.body_text = format!(
                "\n\n---------- 转发的邮件 ----------\n发件人：{from_disp}\n日期：{}\n收件人：{}\n{cc_line}主题：{}\n\n{}\n",
                src.time_full, src.to_addrs, src.subject, src.body_text
            );
        } else {
            let esc = |s: &str| html_escape(s);
            let cc_row = if src.cc_addrs.trim().is_empty() {
                String::new()
            } else {
                format!("<div>抄送：{}</div>", esc(&src.cc_addrs))
            };
            ctx.body_mode = "html".into();
            let head = format!(
                "<p><br></p><p>---------- 转发的邮件 ----------</p>\
                 <div>发件人：{}</div><div>日期：{}</div><div>收件人：{}</div>{cc_row}<div>主题：{}</div>\
                 <p><br></p>",
                esc(&from_disp),
                esc(&src.time_full),
                esc(&src.to_addrs),
                esc(&src.subject)
            );
            ctx.body_html = format!("{head}{}", mail::unwrap_top_div(&src.body_html));
            ctx.body_text = String::new();
        }
    } else {
        // 全新邮件：预填签名档（M3-2，设置→账号维护；留两行空供正文，
        // 光标默认在顶部即正文起点）。回复/转发/草稿编辑不插入（避免重复粘贴）。
        let d = defaults();
        let signature = state.get_or(keys::SIGNATURE, &d).unwrap_or_default();
        if !signature.trim().is_empty() {
            ctx.body_text = format!("\n\n--\n{}", signature.trim_end());
        }
    }
    render_html("compose.html", &ctx)
}

/// 内嵌图片载荷（表单 inline_images JSON 数组项）
#[derive(serde::Deserialize)]
struct InlineImageForm {
    name: String,
    mime: String,
    /// base64（data:image 载荷部分，不带前缀）
    data: String,
}

/// 解析 inline_images 表单域为字节载荷（解码失败剔除并记日志，不阻断保存）
fn parse_inline_images(form: &HashMap<String, String>) -> Vec<mail::InlineImage> {
    let Some(raw) = form.get("inline_images") else {
        return Vec::new();
    };
    let Ok(list) = serde_json::from_str::<Vec<InlineImageForm>>(raw) else {
        eprintln!("[compose] inline_images 解析失败，忽略");
        return Vec::new();
    };
    list.into_iter()
        .filter_map(|f| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(f.data.as_bytes())
                .ok()
                .map(|bytes| mail::InlineImage {
                    filename: format!("image-{}-{}", mail::now_epoch(), f.name),
                    mime: f.mime,
                    bytes,
                })
        })
        .collect()
}

/// 存/覆盖草稿（POST /compose/draft）：表单含 keep_ids/attach_paths（JSON 数组）
fn compose_draft_save(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let form = parse_form(&req.body);
    let ctx_qs = form_list_query(&form);
    let ctx_suffix = if ctx_qs.is_empty() {
        String::new()
    } else {
        format!("&{ctx_qs}")
    };
    match save_compose_form(state, req) {
        Ok((id, skipped, _)) => Resp::hx_redirect(&format!(
            "/compose?draft_id={id}&saved=1{}{ctx_suffix}",
            skipped_suffix(skipped)
        )),
        Err((draft_id, e)) => redirect_compose_ctx(draft_id, &e, &ctx_qs),
    }
}

/// 跳过的空文件数 → 查询串片段（0 时不加参数，保持既有 URL 形态）
fn skipped_suffix(skipped: usize) -> String {
    if skipped == 0 {
        String::new()
    } else {
        format!("&skipped={skipped}")
    }
}

/// 草稿自动保存（M3-1，POST /compose/autosave）：与「存草稿」同一套
/// save_compose_form，但不跳转——返回轻量 JSON（`{"ok":true,"draft_id":N}`），
/// 前端静默回填 draft_id 隐藏域并显示「已自动保存 HH:MM」。
/// 空表单（所有语义字段为空）返回 ok:true 但不落库，避免打开撰写页即生成空草稿。
fn compose_autosave(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return Resp::json("{\"ok\":false}".into());
    }
    let form = parse_form(&req.body);
    let body_empty = |f: &str| form.get(f).map(|v| v.trim()).unwrap_or("").is_empty();
    let trivial = [
        "to_addrs",
        "cc_addrs",
        "bcc_addrs",
        "subject",
        "body_text",
        "body_html",
    ]
    .iter()
    .all(|f| body_empty(f));
    if trivial {
        return Resp::json("{\"ok\":true,\"draft_id\":0}".into());
    }
    match save_compose_form(state, req) {
        Ok((id, _skipped, _converted)) => {
            Resp::json(serde_json::json!({ "ok": true, "draft_id": id }).to_string())
        }
        // 自动保存失败不打断编辑（下次防抖/手动保存重试），前端只提示静默
        Err(_) => Resp::json("{\"ok\":false}".into()),
    }
}

/// 撰写表单公共保存（存草稿/自动保存/发送共用 §9）：落草稿 → keep 裁剪 → 新附件追加。
/// 失败返回 (原 draft_id, 原因) 供回撰写页提示；成功返回 (邮件 id, 被跳过的空文件数)。
/// scheduled_at（M4-2）：datetime-local 原始字符串 → epoch 秒；空/非法按 0 处理
/// （存草稿保留既有定时，清空选择器即取消定时；立即发送路径由调用方写 0）
fn save_compose_form(state: &AppState, req: &Req) -> Result<(i64, usize, usize), (i64, String)> {
    let form = parse_form(&req.body);
    let d = defaults();
    let g = |k: &str| state.get_or(k, &d).unwrap_or_default();
    let draft_id = form_i64(&form, "draft_id");
    // 正文模式：html 模式下按 body_html 源码生成纯文本版（检索/预览）并净化入库
    let body_mode = form.get("body_mode").map(String::as_str).unwrap_or("plain");
    let (body_text, html_raw) = if body_mode == "html" {
        let raw = form.get("body_html").cloned().unwrap_or_default();
        (mail::strip_html(&raw), Some(raw))
    } else {
        (form.get("body_text").cloned().unwrap_or_default(), None)
    };
    // 定时发送（M4-2）：控件原始字符串解析（空 = 非定时；非法格式丢弃并记日志）
    let sched_raw = form.get("scheduled_at").cloned().unwrap_or_default();
    let scheduled_at = if sched_raw.trim().is_empty() {
        0
    } else {
        mail::sched_epoch_of(&sched_raw).unwrap_or_else(|| {
            eprintln!("[compose] scheduled_at 解析失败（按非定时处理）：{sched_raw:?}");
            0
        })
    };
    let input = mail::DraftInput {
        id: draft_id,
        from_addr: full_address(&g(keys::ACCOUNT_PREFIX), &g(keys::ACCOUNT_DOMAIN)),
        from_name: g(keys::DISPLAY_NAME),
        to_addrs: trimmed_field(&form, "to_addrs").unwrap_or_default(),
        cc_addrs: trimmed_field(&form, "cc_addrs").unwrap_or_default(),
        bcc_addrs: trimmed_field(&form, "bcc_addrs").unwrap_or_default(),
        subject: form.get("subject").cloned().unwrap_or_default(),
        body_text,
        in_reply_to: form_i64(&form, "in_reply_to"),
        scheduled_at,
    };
    let keep_ids = form_json_i64s(&form, "keep_ids");
    let paths = form_json_strings(&form, "attach_paths");
    let forward_from = form_i64(&form, "forward_from").unwrap_or(0);
    let id = match html_raw {
        Some(html) => mail::save_draft_with_html(state, &input, &html),
        None => mail::save_draft(state, &input),
    }
    .map_err(|e| (draft_id.unwrap_or(0), e))?;
    // 顺序敏感：keep_ids 裁剪只针对本次保存前已存在的附件，必须排在转发复制之前。
    // 否则首次保存转发时 keep_ids 为空，会把刚复制进来的附件全部删掉（附件永远带不过去）。
    mail::remove_attachments_except(state, id, &keep_ids)
        .map_err(|e| (id, format!("附件裁剪失败：{e}")))?;
    // 转发：仅在首次保存（尚无 draft_id）时把原邮件附件复制到新草稿，
    // 之后的重存由 keep_ids 机制保留（页面会带上已复制附件的 id），不会重复复制
    if draft_id.is_none()
        && forward_from > 0
        && let Err(err) = mail::copy_attachments(state, forward_from, id)
    {
        return Err((id, format!("转发附件复制失败：{err}")));
    }
    // 作为附件转发（B6）：把源邮件重建为 .eml，以 message/rfc822 附件加入草稿。
    // 以「已存在同名 message/rfc822 附件」判定是否已附加：既能防重存重复添加，
    // 也覆盖首次附加失败后重试的场景（此时页面仍带 attach_eml_from）。
    let attach_eml_from = form_i64(&form, "attach_eml_from").unwrap_or(0);
    if attach_eml_from > 0 {
        let name = form
            .get("attach_eml_name")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("corpmail-{attach_eml_from}.eml"));
        let already = mail::get(state, id)
            .map(|d| {
                d.attachments
                    .iter()
                    .any(|a| a.mime == "message/rfc822" && a.filename == name)
            })
            .unwrap_or(false);
        if !already {
            let bytes = mail::eml::build_eml(state, attach_eml_from)
                .map_err(|e| (id, format!("转发邮件生成失败：{e}")))?;
            mail::add_attachment_bytes(state, id, &name, "message/rfc822", &bytes)
                .map_err(|e| (id, format!("转发邮件附加失败：{e}")))?;
        }
    }
    let mut skipped = 0usize;
    if !paths.is_empty() {
        let (_, skipped_names) = mail::add_attachments(state, id, &paths)
            .map_err(|e| (id, format!("附件保存失败：{e}")))?;
        // 0 字节文件被跳过（不再整单失败），数量回传给页面提示
        skipped = skipped_names.len();
    }
    // 超大内嵌图片转附件（前端已把它从正文剥离，此处落库为附件）
    let inlines = parse_inline_images(&form);
    let converted = mail::add_inline_images(state, id, &inlines)
        .map_err(|e| (id, format!("内嵌图片转附件失败：{e}")))?;
    Ok((id, skipped, converted))
}

/// 发送（POST /compose/send §9）：先存最新内容 → 原子排队（重复提交 0 行，幂等 §7）
/// → 入发送队列 → 跳详情页轮询至终态（§2 异步原则）
fn compose_send(state: &AppState, req: &Req) -> Resp {
    if !state.configured() {
        return render_html("welcome.html", &());
    }
    let form = parse_form(&req.body);
    let ctx_qs = form_list_query(&form);
    let mut just_sent = None;
    // 定时发送（M4-2）：scheduled_at 非空且晚于当前 → 不入队。
    // 校验前置（保存前）：过时刻拒绝并回撰写页，避免"存下过去时刻的定时 →
    // 调度线程当轮立即发出"的竞态
    let sched_raw = form.get("scheduled_at").cloned().unwrap_or_default();
    let sched_epoch = if sched_raw.trim().is_empty() {
        None
    } else {
        mail::sched_epoch_of(&sched_raw)
    };
    if let Some(at) = sched_epoch
        && at <= mail::now_epoch() + 60
    {
        return redirect_compose(
            form_i64(&form, "draft_id").unwrap_or(0),
            "定时时间需晚于当前时间 1 分钟以上，请重新选择",
        );
    }
    match save_compose_form(state, req) {
        Ok((id, skipped, converted)) => {
            // 定时（M4-2）：保存已落 scheduled_at（DraftInput），跳过 enqueue；
            // 回草稿详情页展示「已定时」状态（调度线程到点自动入队发送）
            if sched_epoch.is_some() {
                if skipped > 0 {
                    return redirect_compose(
                        id,
                        &format!("已跳过 {skipped} 个 0 字节文件（未附加），请确认附件后再定时"),
                    );
                }
                let mut target = format!("/emails/{id}");
                if !ctx_qs.is_empty() {
                    target.push('?');
                    target.push_str(&ctx_qs);
                }
                return Resp::hx_redirect(&target);
            }
            // 空文件被跳过 → 回撰写页确认；超大图片已转附件 → 提示后正常发出
            if skipped > 0 {
                return redirect_compose(
                    id,
                    &format!("已跳过 {skipped} 个 0 字节文件（未附加），请确认附件后再次发送"),
                );
            }
            if converted > 0 {
                eprintln!("[compose] {converted} 张超大内嵌图片已转为附件随信发送");
            }
            // A1 撤销窗口：不立即入队，先写 scheduled_at = now + 8s（status 仍
            // 为 draft），由调度线程到点派发；窗口内可 POST /undo-send 拦截。
            // 列表页据此启动倒计时 toast（先给撤销按钮，再轮询至终态）。
            if mail::schedule_send_in(state, id, UNDO_SEND_SECS) {
                just_sent = Some(id);
            }
            // 发送完成 → 回来源文件夹列表（保持过滤/翻页），而非停详情页。
            // ctx_qs 恒含 folder（rebuild_list_query 不再省略 inbox），
            // 从哪转发/回复/重新编辑进来就回哪个列表。
            let mut target = if ctx_qs.is_empty() {
                "/".to_string()
            } else {
                format!("/?{ctx_qs}")
            };
            if let Some(id) = just_sent {
                if target.contains('?') {
                    target.push_str(&format!("&just_sent={id}"));
                } else {
                    target.push_str(&format!("?just_sent={id}"));
                }
            }
            Resp::hx_redirect(&target)
        }
        Err((draft_id, e)) => redirect_compose_ctx(draft_id, &e, &ctx_qs),
    }
}

/// 发送状态查询（GET /emails/{id}/status，M1-6）：列表页发送反馈 toast 轮询用。
/// 轻量 JSON（status + error）而非整页片段——轮询高频，响应体控制到最小
fn email_status(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    match state.with_conn(|c| {
        c.query_row(
            "SELECT status, COALESCE(error, '') FROM emails WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
    }) {
        Ok((status, error)) => {
            Resp::json(serde_json::json!({ "status": status, "error": error }).to_string())
        }
        Err(_) => Resp::not_found(),
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

/// 同 redirect_compose，但把列表上下文带回撰写页（保存失败重试后返回链接不退化）
fn redirect_compose_ctx(draft_id: i64, error: &str, ctx_qs: &str) -> Resp {
    if ctx_qs.is_empty() {
        return redirect_compose(draft_id, error);
    }
    Resp::hx_redirect(&format!(
        "/compose?draft_id={draft_id}&error={}&{ctx_qs}",
        urlencode(error)
    ))
}

/// Office 预览（B8，GET /emails/{id}/attachments/{aid}/office）：
/// 大小检查 → 格式判定 → 清理宏/OLE/外部关系后回清洗副本（前端本地渲染）。
fn attachment_office(state: &AppState, req: &Req) -> Resp {
    let Some((email_id, aid)) = path_attach(&req.path) else {
        return Resp::not_found();
    };
    let Some((view, bytes)) = mail::attachment_bytes(state, email_id, aid) else {
        return Resp::not_found();
    };
    let max = mail::office::preview_max_bytes(state);
    if bytes.len() as u64 > max {
        return office_error_resp(
            413,
            &format!(
                "文件大小 {:.1}MB 超过预览上限 {}MB；请下载后用系统应用打开。",
                bytes.len() as f64 / 1024.0 / 1024.0,
                max / 1024 / 1024
            ),
        );
    }
    match mail::office::detect(&view.mime, &view.filename, &bytes) {
        Ok(kind) => match mail::office::sanitize_for_preview(&bytes, kind) {
            Ok(clean) => Resp {
                status: 200,
                content_type: "application/octet-stream".into(),
                cache_control: "no-store",
                body: clean,
                extra_headers: vec![
                    ("X-Content-Type-Options".into(), "nosniff".into()),
                    ("Content-Disposition".into(), "inline".into()),
                ],
            },
            Err(e) => office_error_resp(e.http_status(), &e.message()),
        },
        Err(e) => office_error_resp(e.http_status(), &e.message()),
    }
}

/// Office 纯文本兜底（B8，GET /emails/{id}/attachments/{aid}/office-text）：
/// DOCX/XLSX/XLS 抽取正文；PPTX 按产品要求不降级（前端提示下载）。
fn attachment_office_text(state: &AppState, req: &Req) -> Resp {
    let Some((email_id, aid)) = path_attach(&req.path) else {
        return Resp::not_found();
    };
    let Some((view, bytes)) = mail::attachment_bytes(state, email_id, aid) else {
        return Resp::not_found();
    };
    let max = mail::office::preview_max_bytes(state);
    if bytes.len() as u64 > max {
        return office_error_resp(
            413,
            &format!(
                "文件大小 {:.1}MB 超过预览上限 {}MB；请下载后用系统应用打开。",
                bytes.len() as f64 / 1024.0 / 1024.0,
                max / 1024 / 1024
            ),
        );
    }
    let kind = match mail::office::detect(&view.mime, &view.filename, &bytes) {
        Ok(k) => k,
        Err(e) => return office_error_resp(e.http_status(), &e.message()),
    };
    if kind == mail::office::OfficeKind::Pptx {
        return office_error_resp(
            415,
            "PPTX 不提供文本预览；请下载后用 Microsoft PowerPoint 打开。",
        );
    }
    match mail::office::extract_text(&bytes, kind) {
        Ok(text) if !text.trim().is_empty() => Resp {
            status: 200,
            content_type: "text/plain; charset=utf-8".into(),
            cache_control: "no-store",
            body: text.into_bytes(),
            extra_headers: vec![("X-Content-Type-Options".into(), "nosniff".into())],
        },
        Ok(_) => office_error_resp(422, "未能从文件中提取到可预览的文本。"),
        Err(e) => office_error_resp(e.http_status(), &e.message()),
    }
}

/// 预览错误统一为 text/plain，前端直接展示 body
fn office_error_resp(status: u16, message: &str) -> Resp {
    Resp {
        status,
        content_type: "text/plain; charset=utf-8".into(),
        cache_control: "no-store",
        body: message.as_bytes().to_vec(),
        extra_headers: vec![("X-Content-Type-Options".into(), "nosniff".into())],
    }
}

/// 附件预览（GET /emails/{id}/attachments/{aid}/view，M5）：解压后按原始 MIME 内联返回。
/// 安全约束：邮件头 MIME 不可信（可含控制字符/伪造类型）→ 白名单校验后回退 octet-stream；
/// text 类强制 text/plain（防 HTML 脚本执行）；SVG 不在图片白名单（顶层渲染可执行脚本）；
/// 非内联类型附 Content-Disposition: attachment（强制下载）。
fn attachment_view(state: &AppState, req: &Req) -> Resp {
    let Some((email_id, aid)) = path_attach(&req.path) else {
        return Resp::not_found();
    };
    match mail::attachment_bytes(state, email_id, aid) {
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
    let Some((email_id, aid)) = path_attach(&req.path) else {
        return Resp::not_found();
    };
    let form = parse_form(&req.body);
    let Some(dest) = trimmed_field(&form, "dest") else {
        return Resp::html(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">未选择保存位置</div>"#.into(),
        );
    };
    match mail::save_attachment_to(state, email_id, aid, &dest) {
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

/// 全部另存（A4，POST /emails/{id}/attachments/all）：前端选目录后逐附件落盘，
/// 同名自动加序号不覆盖；部分附件已清理时在提示里报告跳过数。
fn attachments_save_all(state: &AppState, req: &Req) -> Resp {
    let Some(id) = path_id(&req.path, "/emails/") else {
        return Resp::not_found();
    };
    let form = parse_form(&req.body);
    let Some(dir) = trimmed_field(&form, "dest_dir") else {
        return Resp::html(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">未选择保存目录</div>"#.into(),
        );
    };
    match mail::save_all_attachments(state, id, &dir) {
        Ok((saved, 0)) => Resp::html(format!(
            r#"<div id="toast" class="toast ok" hx-swap-oob="true">已保存 {saved} 个附件</div>"#
        )),
        Ok((saved, skipped)) => Resp::html(format!(
            r#"<div id="toast" class="toast ok" hx-swap-oob="true">已保存 {saved} 个附件（{skipped} 个已清理/缺失，跳过）</div>"#
        )),
        Err(e) => Resp::html(format!(
            r#"<div id="toast" class="toast err" hx-swap-oob="true">{}</div>"#,
            html_escape(&e)
        )),
    }
}

// ---------- M2 辅助 ----------

/// 保存路径后缀补全：不以指定扩展名结尾（大小写不敏感）则追加。
/// 系统保存对话框改过文件名或手输路径时，落盘文件可能没有后缀，
/// 双击无法关联应用打开；导出路径的最终形态以补全后为准
fn ensure_ext(dest: &str, ext: &str) -> String {
    if dest.to_ascii_lowercase().ends_with(ext) {
        dest.to_string()
    } else {
        format!("{dest}{ext}")
    }
}

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
        smtp_security: g(keys::SMTP_SECURITY),
        recv_host: g(keys::RECV_HOST),
        recv_port: g(keys::RECV_PORT),
        recv_security: g(keys::RECV_SECURITY),
        first_fetch_days: g(keys::FIRST_FETCH_DAYS),
        // 目录预填与设置·存储页同源：迁移待生效时显示目标目录，否则当前目录
        //（用户反馈：向导设置的目录与设置页显示不一致）
        data_dir: state
            .pending_data_dir()
            .unwrap_or_else(|| state.data_dir().display().to_string()),
        cred_note: cred_storage_note(),
        is_ct_domain: is_ct_mail_domain(&g(keys::ACCOUNT_DOMAIN)),
    };
    render_html("setup.html", &ctx)
}

/// 是否集团统一邮箱域名（chinatelecom.cn）：需在 OA 预先生成客户端专用密码、
/// 开启客户端收取选项，向导/设置页据此展示指引文案
/// 是否集团统一邮箱域名（chinatelecom.cn）：需在 OA 预先生成客户端专用密码、
/// 开启客户端收取选项，向导/设置页据此展示指引文案（settings.rs 复用）
pub(crate) fn is_ct_mail_domain(domain: &str) -> bool {
    domain
        .trim()
        .trim_start_matches('@')
        .eq_ignore_ascii_case("chinatelecom.cn")
}

/// 凭据保管文案（编译期平台自适应；模板不再写死两个平台名）
fn cred_storage_note() -> String {
    if cfg!(target_os = "macos") {
        "🔒 密码保存进 macOS 钥匙串，服务器与邮件数据保存在本机数据库。".into()
    } else if cfg!(target_os = "windows") {
        "🔒 密码保存进 Windows 凭据管理器，服务器与邮件数据保存在本机数据库。".into()
    } else {
        "🔒 密码保存进系统凭据保管，服务器与邮件数据保存在本机数据库。".into()
    }
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
        dir_changed: (state.data_dir() != state.anchor_dir().to_path_buf())
            .then(|| state.data_dir().display().to_string()),
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
    // 加密方式：默认按端口推断（465/993 隐式 TLS，其余 STARTTLS），可显式覆盖
    let smtp_security = security_or(&form, state, keys::SMTP_SECURITY);
    let recv_security = security_or(&form, state, keys::RECV_SECURITY);
    let smtp_mode = crate::net::TlsMode::resolve(smtp_port, &smtp_security);
    let recv_mode = crate::net::TlsMode::resolve(recv_port, &recv_security);

    // SMTP/IMAP 并行测试：两通道互不依赖，串行最坏 2×15s 超时（内网拦 TLS
    // 不回 RST 的场景下长达 30s 无反馈）；并行把最坏等待压到单通道上限。
    // scope 借用 tester/参数（无 'static 要求）；线程 panic 兑底为失败结果
    let (smtp, imap) = std::thread::scope(|s| {
        let smtp_handle =
            s.spawn(|| tester.smtp(&smtp_host, smtp_port, smtp_mode, &account, &password));
        let imap_handle =
            s.spawn(|| tester.imap(&recv_host, recv_port, recv_mode, &account, &password));
        let panicky = |label: &str| TestOutcome {
            ok: false,
            elapsed_ms: 0,
            detail: format!("{label}测试线程异常"),
        };
        let smtp = smtp_handle.join().unwrap_or_else(|_| panicky("发信"));
        let imap = imap_handle.join().unwrap_or_else(|_| panicky("收信"));
        (smtp, imap)
    });
    if smtp.ok && imap.ok {
        // 首次邮件拉取范围（7/30/90/全部=0，非法回落 30）
        let first_fetch = match form.get("first_fetch_days") {
            Some(v) if matches!(v.as_str(), "7" | "30" | "90" | "0") => v.clone(),
            _ => "30".to_string(),
        };
        let display = trimmed_field(&form, "display_name").unwrap_or_default();
        let smtp_port_s = smtp_port.to_string();
        let recv_port_s = recv_port.to_string();
        let pairs: Vec<(&str, &str)> = vec![
            (keys::ACCOUNT_PREFIX, prefix.as_str()),
            (keys::ACCOUNT_DOMAIN, &domain),
            (keys::DISPLAY_NAME, display.as_str()),
            (keys::SMTP_HOST, smtp_host.as_str()),
            (keys::SMTP_PORT, smtp_port_s.as_str()),
            (keys::SMTP_SECURITY, smtp_security.as_str()),
            (keys::RECV_HOST, recv_host.as_str()),
            (keys::RECV_PORT, recv_port_s.as_str()),
            (keys::RECV_SECURITY, recv_security.as_str()),
            (keys::FIRST_FETCH_DAYS, first_fetch.as_str()),
        ];
        // 首设空库直切：未配置且库无任何数据时，自定义目录不走迁移——
        // 迁移的快照/复制/重启对空库全是无谓成本，直切在会话内立即生效。
        // 必须排在 set_batch 之前：配置随后写入的才是新库（与迁移路径
        // "先保存后快照"互补，空库没有可快照的内容）。
        let first_run_fresh = !state.configured() && state.store_is_empty();
        let data_dir_input = trimmed_field(&form, "data_dir").unwrap_or_default();
        // 向导把 data_dir 预填成当前目录（与存储页同源回显）：用户不动它 =
        // 不更换，不得送进直切校验——validate_data_dir 拒绝同路径，
        // 曾导致首配不动目录直接提交必然失败且不跳转 done 页。
        // 归一化方式与下方迁移路径的 want/cur 比较保持一致。
        let data_dir_unchanged = !data_dir_input.is_empty()
            && data_dir_input.trim().trim_end_matches(['/', '\\'])
                == state
                    .data_dir()
                    .to_string_lossy()
                    .trim_end_matches(['/', '\\']);
        if first_run_fresh
            && !data_dir_input.is_empty()
            && !data_dir_unchanged
            && let Err(e) = state.switch_data_dir_first_run(&data_dir_input)
        {
            return test_fragment(
                Vec::new(),
                &format!("数据目录设置失败：{e}（可清空目录后重新提交）"),
                false,
            );
        }
        // C2/C6：配置与密码先落库，迁移最后执行——
        // C2：first_fetch_days 存 DB（非 corpmail.toml 键），若迁移在前，快照发生于
        //     落库之前，重启后新目录丢首拉范围回退默认 30；先保存再迁移，VACUUM
        //     快照即包含最新值；
        // C6：迁移最后执行，失败时账号配置已完整保存（无半保存态），指针未推进，
        //     目标残留已被 move_data_dir 清理，可清空目录字段重提或稍后在
        //     设置→存储管理 重试更换位置
        // （首设直切路径：以下落库写入的就是新目录的库）
        if let Err(e) = state.set_batch(&pairs) {
            return test_fragment(Vec::new(), &format!("保存失败：{e}"), false);
        }
        if let Err(e) = state.store_password(&account, &password) {
            return test_fragment(Vec::new(), &format!("密码保存失败：{e}"), false);
        }
        // 数据保存目录（非首设路径）：非空且不同于当前时迁移；
        // 已是"迁移完成待重启"或"首设已直切"状态则跳过
        if !first_run_fresh && !data_dir_input.is_empty() {
            let cur = state
                .data_dir()
                .to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string();
            let want = data_dir_input
                .trim()
                .trim_end_matches(['/', '\\'])
                .to_string();
            // 指针已指向 want 且库文件已存在 = 迁移待重启或直切已完成 → 幂等跳过
            let already_settled = state
                .get(keys::DATA_DIR)
                .flatten()
                .is_some_and(|p| p.trim_end_matches(['/', '\\']) == want)
                && std::path::Path::new(&want).join("corpmail.db").is_file();
            if want != cur
                && !already_settled
                && let Err(e) = state.move_data_dir(&data_dir_input)
            {
                return test_fragment(
                    Vec::new(),
                    &format!(
                        "账号已保存，但数据目录迁移失败：{e}（可清空目录后重新提交，或稍后在 设置 → 存储管理 更换位置）"
                    ),
                    false,
                );
            }
        }
        // 首次同步：配置完成立即拉取（fetch_round 内部先发件箱、后收件箱），
        // 不必等下一个定时周期。启动调度线程的 2s 首收跑在向导之前，
        // 那时未配置已被 start_fetch 静默忽略。
        // 数据目录迁移已完成待重启时跳过——本轮会写入旧目录，重启后新库
        // 由 sync_on_start 再拉一次，避免白拉
        if state.pending_data_dir().is_none() {
            state.start_fetch();
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
        inbox: counts["inbox"],
        inbox_unread: counts["inbox_unread"],
        all_mail: counts["all"],
        sent: counts["sent"],
        draft: counts["draft"],
        folder: folder.into(),
    }
}

/// 详情页「返回列表」链接：原样还原进入前的过滤状态。
/// 参数来自列表行链接（见 MainCtx.list_query）；未携带时按邮件所在 folder。
/// 全部走白名单/夹取：q 需 urlencode（用户输入），folder 只放行已知值，
/// 页码夹取到 1..=10 万防畸形查询串撑爆。
fn list_query_of(req: &Req, fallback_folder: &str) -> String {
    let get = |name: &str| req.query_param(name);
    let folder = match get("folder").as_deref() {
        Some("all") | Some("sent") | Some("draft") | Some("inbox") => get("folder").unwrap(),
        _ => fallback_folder.to_string(),
    };
    let q = get("q");
    let page = get("page");
    let mode = get("mode");
    let unread = get("unread");
    let external = get("external");
    let per = get("per");
    let sort = get("sort");
    let from = get("from");
    let df = get("df");
    let dt = get("dt");
    let att = get("att");
    rebuild_list_query(
        &folder,
        mode.as_deref(),
        unread.as_deref(),
        external.as_deref(),
        q.as_deref(),
        page.as_deref(),
        per.as_deref(),
        sort.as_deref(),
        from.as_deref(),
        df.as_deref(),
        dt.as_deref(),
        att.as_deref(),
    )
}

/// 列表上下文查询串还原（详情页「返回列表」/撰写页回跳/批量操作重建列表
/// 请求共用）：folder 恒带，unread/external/q/sort 按需；per/sort 仅非默认
/// 时携带（默认 10 条/时间倒序时 URL 保持干净，详见 list_root per 解析注释）。
/// 入口有三：Req 查询串（list_query_of）、表单隐藏域（发送/存草稿回跳）。
/// 表单值客户端可控，重新过滤防注入畸形查询串
#[allow(clippy::too_many_arguments)]
fn rebuild_list_query(
    folder: &str,
    mode: Option<&str>,
    unread: Option<&str>,
    external: Option<&str>,
    q: Option<&str>,
    page: Option<&str>,
    per: Option<&str>,
    sort: Option<&str>,
    from: Option<&str>,
    df: Option<&str>,
    dt: Option<&str>,
    att: Option<&str>,
) -> String {
    let folder = match folder {
        "all" | "sent" | "draft" | "inbox" => folder.to_string(),
        _ => "inbox".to_string(),
    };
    let mut qs: Vec<String> = Vec::new();
    // 恒携带 folder（含 inbox）：发送/存草稿回跳详情页、详情页「返回列表」、
    // 列表链接都依赖 folder 明确，省略会让回跳退到邮件自身 folder 而错落
    // 草稿箱/发件箱。收件箱显式 folder=inbox 只是 URL 更明确，语义不变。
    qs.push(format!("folder={folder}"));
    // 会话视图仅收件箱/全部可用；其他箱体即使带 mode=thread 也无意义，丢弃
    if mode == Some("thread") && matches!(folder.as_str(), "inbox" | "all") {
        qs.push("mode=thread".into());
    }
    if unread == Some("1") {
        qs.push("unread=1".into());
    }
    if external == Some("1") {
        qs.push("external=1".into());
    }
    if let Some(q) = q.filter(|v| !v.trim().is_empty()) {
        qs.push(format!("q={}", urlencode(q)));
    }
    if let Some(p) = clamp_per(per)
        && p != PER_DEFAULT
    {
        qs.push(format!("per={p}"));
    }
    // 排序：仅非默认值携带（默认 date_desc 不污染 URL）
    let sort = mail::SortKey::parse(sort);
    if sort != mail::SortKey::DateDesc {
        qs.push(format!("sort={}", sort.as_str()));
    }
    // 高级检索（B1）：发件人/日期/附件，非法日期丢弃（与 list_root 同口径）
    if let Some(f) = from.map(str::trim).filter(|v| !v.is_empty()) {
        qs.push(format!("from={}", urlencode(f)));
    }
    if let Some(d) = df.filter(|v| mail::date_epoch_of(v, false).is_some()) {
        qs.push(format!("df={}", urlencode(d)));
    }
    if let Some(d) = dt.filter(|v| mail::date_epoch_of(v, true).is_some()) {
        qs.push(format!("dt={}", urlencode(d)));
    }
    if att == Some("1") {
        qs.push("att=1".into());
    }
    let page = clamp_page(page);
    if page > 1 {
        qs.push(format!("page={page}"));
    }
    qs.join("&")
}

/// URL page 参数归一：非法 → 1，上限 100000。
/// list_root（列表渲染）与 rebuild_list_query（上下文回填）共用同一口径
///（审计 I3）：此前列表侧只 max(1) 无上限，极端大页码会造成巨大 OFFSET
/// 扫描，且与回填侧 clamp 后的页码往返不一致导致前后页跳动
fn clamp_page(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, 100_000)
}

/// 每页条数下限（1 = 分页功能的最小可验证粒度，E2E B8 用 per=1 测翻页）
/// 与上限（防恶意大值一次拉全表）。合法选项 10/20/50/100，clamp 而非白名单：
/// 直链任意值也能得到合理表现
const PER_MIN: i64 = 1;
const PER_MAX: i64 = 100;
/// 默认每页条数（settings 缺省值；URL 洁净阈值——per 等于默认值时不进
/// 查询串，与 mail_list.html 的 `{% if per != 10 %}` 保持同一口径）
const PER_DEFAULT: i64 = 10;

/// 撤销发送窗口（A1）：点发送后先等这么多秒才真正入队，
/// 窗口内 toast 提供「撤销发送」。与 app.js 的倒计时保持一致。
const UNDO_SEND_SECS: i64 = 8;

/// URL per 参数归一：非法/缺省 → None，越界钳到 [PER_MIN, PER_MAX]
fn clamp_per(raw: Option<&str>) -> Option<i64> {
    raw.and_then(|v| v.parse::<i64>().ok())
        .map(|v| v.clamp(PER_MIN, PER_MAX))
}

/// settings 中每页条数（缺省/非法 → 10；越界钳制）
fn per_from_settings(state: &AppState) -> i64 {
    let d = defaults();
    state
        .get_or(keys::PAGE_SIZE, &d)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(PER_DEFAULT)
        .clamp(PER_MIN, PER_MAX)
}

/// 表单隐藏域 list_query（compose 页透传的进入前列表上下文）重新过滤——
/// 客户端可控，不可直接拼 URL。串形态即 list_query_of 产出（k=v&…），
/// 拆开重走白名单，畸形值全部丢弃
fn form_list_query(form: &HashMap<String, String>) -> String {
    let Some(raw) = form.get("list_query") else {
        return String::new();
    };
    let pairs: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let pick = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    rebuild_list_query(
        pick("folder").unwrap_or("inbox"),
        pick("mode"),
        pick("unread"),
        pick("external"),
        pick("q"),
        pick("page"),
        pick("per"),
        pick("sort"),
        pick("from"),
        pick("df"),
        pick("dt"),
        pick("att"),
    )
}

/// 详情页「返回列表」链接：原样还原进入前的过滤状态。
/// 参数来自列表行链接（见 MainCtx.list_query）；未携带时按邮件所在 folder。
fn detail_back_href(req: &Req, fallback_folder: &str) -> String {
    let qs = list_query_of(req, fallback_folder);
    if qs.is_empty() {
        "/".to_string()
    } else {
        format!("/?{qs}")
    }
}

/// 回复全部的收件人串：发件人 + 收件人 + 抄送，去掉自己（大小写不敏感）、
/// 去重保序；有姓名按 "Name <addr>" 回填（联系人库识别更友好）
/// 本账号完整地址（小写）。回复/回复全部过滤"自己"用
fn own_address(state: &AppState) -> String {
    let d = defaults();
    full_address(
        &state.get_or(keys::ACCOUNT_PREFIX, &d).unwrap_or_default(),
        &state.get_or(keys::ACCOUNT_DOMAIN, &d).unwrap_or_default(),
    )
    .to_ascii_lowercase()
}

/// 多个原始地址串 → 剔除自己与 exclude 集、去重、保序，返回 ", " 拼接的展示串
/// （保留 "姓名 <地址>" 显示名）。exclude 为已进收件人的小写地址，避免 To/Cc 重复。
fn merge_addrs(raw_sets: &[&str], self_addr: &str, exclude: &[String]) -> String {
    let mut seen: Vec<String> = exclude.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut parts: Vec<String> = Vec::new();
    for raw in raw_sets {
        for (addr, name) in mail::contacts::parse_addr_list(raw) {
            let addr = addr.trim().to_string();
            if addr.is_empty() || addr.to_ascii_lowercase() == self_addr {
                continue;
            }
            let key = addr.to_ascii_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            parts.push(if name.trim().is_empty() {
                addr
            } else {
                format!("{name} <{addr}>")
            });
        }
    }
    parts.join(", ")
}

/// 回复全部拆分：收件人 = 原发件人+原收件人（去自己）；抄送 = 原抄送
/// （去自己、去掉已进收件人的、去重）——原抄送不再"全塞进收件人"（M9）
fn reply_all_parts(state: &AppState, src: &mail::EmailDetail) -> (String, String) {
    let self_addr = own_address(state);
    // 原发件人保留显示名（"姓名 <地址>"），收件人/抄送串按原样解析
    let from_disp = if src.from_name.trim().is_empty() {
        src.from_addr.clone()
    } else {
        format!("{} <{}>", src.from_name, src.from_addr)
    };
    let to = merge_addrs(&[&from_disp, &src.to_addrs], &self_addr, &[]);
    let to_keys: Vec<String> = mail::contacts::parse_addr_list(&to)
        .into_iter()
        .map(|(a, _)| a.to_ascii_lowercase())
        .collect();
    let cc = merge_addrs(&[&src.cc_addrs], &self_addr, &to_keys);
    (to, cc)
}

/// 地址串拆成展示片段（详情页收件人/抄送逐个标注外部）
fn addr_chips(raw: &str, domain: &str) -> Vec<AddrChip> {
    mail::contacts::parse_addr_list(raw)
        .into_iter()
        .map(|(addr, name)| AddrChip {
            text: if name.is_empty() {
                addr.clone()
            } else {
                format!("{name} <{addr}>")
            },
            external: mail::is_external_addr(&addr, domain),
        })
        .collect()
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

/// 渲染片段并附带侧栏刷新事件：清理/删除等改变文件夹计数的动作，
/// 返回的 pane 只换主区，边栏计数停在旧值——由 HX-Trigger 派发
/// mbx:fetchDone（app.js 既有的局部刷新通道：side+list，不整页 reload）
pub(super) fn render_pane_with_sidebar_refresh(name: &str, ctx: &impl Serialize) -> Resp {
    render_html(name, ctx).with_event("mbx:fetchDone")
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

/// 加密方式字段：表单值优先（仅接受四个合法串），否则回落到已保存值。
/// 返回规范化的配置串（auto|implicit|starttls|plain）
pub(super) fn security_or(form: &HashMap<String, String>, state: &AppState, key: &str) -> String {
    let d = defaults();
    match form.get(key).map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "auto" | "implicit" | "starttls" | "plain") => v,
        _ => state
            .get_or(key, &d)
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| state::SECURITY_DEFAULT.to_string()),
    }
}

pub(super) const SMTP_PORT_DEFAULT: u16 = 465;
pub(super) const RECV_PORT_DEFAULT: u16 = 993;

/// 文件大小人性化（设置页 DB/附件/备份/预估统一入口；B 起步，GB/TB 也覆盖，
/// 与模板 fmt_size 过滤器同逻辑，全局一致）
pub(super) fn human_bytes(b: u64) -> String {
    templates::fmt_size(b as i64)
}

#[cfg(test)]
#[path = "../tests/ui_router.rs"]
mod tests;
