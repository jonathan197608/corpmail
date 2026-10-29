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

mod clean;
mod conf;
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
            // 状态在窗口创建前装配：协议回调依赖 AppState（设计 §2）。
            // M5 自定义数据目录：先开锚点（系统默认目录）库读 data_dir_override
            // 指针，指针有效（目录存在）则定位用户目录，否则安全回退锚点目录
            let anchor = app.path().app_data_dir()?;
            let data_dir = state::AppState::resolve_start_dir(&anchor);
            let state = state::AppState::open_with_anchor(&data_dir, &anchor);
            // M3 发送队列：单飞 worker（崩溃残留 queued 补发），窗口创建前就绪
            state.start_send_worker();
            app.manage(state);

            // M4 收件调度（§6.2）：启动 2s 首收（sync_on_start 默认开）+ sync_interval_min 循环。
            // 调度线程持 AppHandle 取 State（设置每轮重读，改参数即时生效；0=仅手动）
            let handle = app.app_handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let state = handle.state::<state::AppState>();
                let on_start = state
                    .get(state::keys::SYNC_ON_START)
                    .flatten()
                    .map(|v| v != "false")
                    .unwrap_or(true);
                if on_start {
                    state.start_fetch();
                }
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    let state = handle.state::<state::AppState>();
                    let only_manual = state
                        .get(state::keys::SYNC_INTERVAL_MIN)
                        .flatten()
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(30);
                    if only_manual > 0 {
                        std::thread::sleep(std::time::Duration::from_secs(
                            only_manual.saturating_sub(1).saturating_mul(60),
                        ));
                        state.start_fetch();
                    }
                }
            });

            // M5 存储清理器（§5.2）：启动 30s 首清（避开启动高峰）+ 每 24h 循环；
            // 策略每轮重读（设置页改保留天数即时生效）；失败仅记日志，下轮再试
            let clean_handle = app.app_handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                loop {
                    let state = clean_handle.state::<state::AppState>();
                    if let Err(err) = clean::run(&state) {
                        eprintln!("[clean] 清理器执行失败：{err}");
                    }
                    std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
                }
            });

            let url: tauri::Url = APP_URL.parse().expect("入口 URL 为常量，恒合法");
            // 主窗口在运行时创建：入口 URL 依赖平台（自定义协议映射差异），配置文件表达不了
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("Mailbox")
                // 默认 1440×900（M5 实测 15 页面零滚动条；内容最高需求 812px @1280 宽）
                .inner_size(1440.0, 900.0)
                .min_inner_size(560.0, 400.0)
                // 启动等待动画配套（M5）：窗口底色=应用背景色，消除 WebView
                // 初始化期（Windows 首启 1-3s）的刺眼纯白；首帧由 base.html
                // 内联 #boot spinner 承接
                .background_color(tauri::webview::Color(0xf2, 0xf4, 0xf8, 0xff))
                .resizable(true)
                .center()
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Mailbox 运行失败");
}
