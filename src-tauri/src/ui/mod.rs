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
pub use router::{RealTester, Req, Resp, route};

/// 平台无关路由结果 → 协议层 HTTP 响应
pub fn to_http_response(resp: Resp) -> Response<Cow<'static, [u8]>> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(resp.status).expect("状态码取值 200/404/500，恒合法"))
        .header(header::CONTENT_TYPE, resp.content_type.as_str())
        .header(header::CACHE_CONTROL, resp.cache_control);
    // 附加头（HX-Redirect 整页跳转等 htmx 约定）
    for (name, value) in &resp.extra_headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Cow::Owned(resp.body))
        .expect("合法头构造，恒成功")
}
