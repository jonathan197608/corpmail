//! Mailbox - Tauri 2 桌面应用入口
//!
//! 架构：单一自定义协议 `ui://` 承载全部页面与片段——
//! - 全部 HTML 由 Rust 端 minijinja 渲染，前端只用 htmx 表达交互
//! - 静态资源（htmx.min.js / app.css）与模板均在编译期内嵌进二进制，离线可用
//! - 不监听任何本地端口，无防火墙弹窗
//! - SQLite（mailbox.db）存于 app_data_dir；账号密码走 OS 凭据保管（设计 §5.4）
//!
//! Windows 平台上，wry/WebView2 将自定义协议映射为 `http://<scheme>.localhost`，
//! macOS/Linux 直接使用原生 scheme，二者对页面同源、无 CORS 差异。

mod conncheck;
mod db;
mod mail;
mod secrets;
mod state;
mod templates;
mod ui;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// 主窗口入口 URL：自定义协议 `ui` 的根路径。
///
/// 平台差异（wry 约定）：Windows(WebView2) 上自定义协议以 `http://ui.localhost/`
/// 暴露并经 WebResourceRequested 过滤器分发（与官方 `http://tauri.localhost` 同机制）；
/// macOS(WKWebView)/Linux(WebKitGTK) 上为 `ui://localhost/`。
#[cfg(windows)]
const APP_URL: &str = "http://ui.localhost/";
#[cfg(not(windows))]
const APP_URL: &str = "ui://localhost/";

/// 应用主入口
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .register_uri_scheme_protocol("ui", |ctx, request| {
            let app = ctx.app_handle();
            let state = app.state::<state::AppState>();
            let assets = ui::EmbeddedAssets::new(app);
            let req = ui::Req::parse_from(&request);
            // debug 构建打印协议层请求（GUI 无控制台时也可用 stderr 日志观察）
            #[cfg(debug_assertions)]
            eprintln!(
                "[ui] {} {}?{} hx={}",
                req.method, req.path, req.query, req.is_htmx_request
            );
            ui::to_http_response(ui::route(&state, &assets, &req, &ui::RealTester))
        })
        .setup(|app| {
            // 状态在窗口创建前装配：协议回调依赖 AppState（设计 §2）
            let data_dir = app.path().app_data_dir()?;
            let state = state::AppState::open(&data_dir);
            // M3 发送队列：单飞 worker（崩溃残留 queued 补发），窗口创建前就绪
            state.start_send_worker();
            app.manage(state);

            let url: tauri::Url = APP_URL.parse().expect("入口 URL 为常量，恒合法");
            // 主窗口在运行时创建：入口 URL 依赖平台（自定义协议映射差异），配置文件表达不了
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("Mailbox")
                .inner_size(960.0, 620.0)
                .min_inner_size(560.0, 400.0)
                .resizable(true)
                .center()
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Mailbox 运行失败");
}
