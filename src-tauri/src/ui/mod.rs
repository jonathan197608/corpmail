//! ui 子系统：自定义协议 `ui://` 的路由与响应
//!
//! 请求经 `Req` 脱壳为平台无关形态，路由产出平台无关 `Resp`，
//! 再由 `to_http_response` 装配为协议层 HTTP 响应（tauri::http）。

mod assets;
mod router;
mod settings;

use std::borrow::Cow;
use tauri::http::{Response, StatusCode, header};

pub use assets::{Assets, EmbeddedAssets};
pub use router::{MAX_REQUEST_BODY, RealTester, Req, Resp, route};

/// 后台迁移已启动的响应标记：前端据此保留阻塞模态等事件；未带此头的 200
/// 响应一律是完整 pane（同步完成或启动校验失败），必须渲染后关模态（审计 A1）。
pub const MIGRATION_BACKGROUND_HEADER: &str = "X-CorpMail-Migration";

/// 内容安全策略（审计 R7）。
///
/// 与 `tauri.conf.json` 的 `app.security.csp` 必须逐字一致——单测
/// `csp_header_matches_tauri_conf` 直接比对，防两处漂移。这里下发响应头而不是
/// 只信配置，是因为 wry 只对「自己加载的文档」应用 config 里的 CSP，自定义
/// 协议经 `to_http_response` 返回的响应在上游没有这道保障。
///
/// 约束：`script-src 'self'` 禁止内联脚本与事件属性，模板里的 `on*` 已全部
/// 迁到 `data-mbx-action` 委托（app.js）；`style-src` 保留 `'unsafe-inline'`，
/// 因为 wangEditor 与内联 `style="..."` 需要。
pub const CSP_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; frame-src 'self' blob:; object-src 'none'; connect-src 'self'";

/// 平台无关路由结果 → 协议层 HTTP 响应
pub fn to_http_response(resp: Resp) -> Response<Cow<'static, [u8]>> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(resp.status).expect("状态码取值 200/404/500，恒合法"))
        .header(header::CONTENT_TYPE, resp.content_type.as_str())
        .header(header::CACHE_CONTROL, resp.cache_control);
    // HTML（整页与 htmx 片段）一律带 CSP：片段虽不新建文档，但同源策略
    // 按文档生效，统一加头免得分支漏掉
    if resp.content_type.starts_with("text/html") {
        builder = builder.header(header::CONTENT_SECURITY_POLICY, CSP_POLICY);
    }
    // 附加头（HX-Redirect 整页跳转等 htmx 约定）
    for (name, value) in &resp.extra_headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Cow::Owned(resp.body))
        .expect("合法头构造，恒成功")
}

#[cfg(test)]
#[path = "../tests/ui_csp.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/ui_migration.rs"]
mod migration_tests;
