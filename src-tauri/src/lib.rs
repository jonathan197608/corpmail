//! CorpMail - Tauri 2 桌面应用入口
//!
//! 架构：单一自定义协议 `ui://` 承载全部页面与片段——
//! - 全部 HTML 由 Rust 端 minijinja 渲染，前端只用 htmx 表达交互
//! - 静态资源（htmx.min.js / app.css）与模板均在编译期内嵌进二进制，离线可用
//! - 不监听任何本地端口，无防火墙弹窗
//! - SQLite（corpmail.db）存于 app_data_dir；账号密码走 OS 凭据保管（设计 §5.4）
//!
//! Windows 平台上，wry/WebView2 将自定义协议映射为 `http://<scheme>.localhost`，
//! macOS/Linux 直接使用原生 scheme，二者对页面同源、无 CORS 差异。

mod clean;
mod conf;
mod conncheck;
mod db;
mod mail;
mod net;
mod secrets;
mod state;
mod templates;
mod ui;

/// 错误转字符串（各模块 `map_err` 适配器共用；避免每个模块各写一份 `fn e`）
pub(crate) fn err<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

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
        // 单实例（首个注册，确保在其他插件初始化前拦截第二实例；
        // 重复启动时聚焦已有主窗口而非新开，避免双实例并发写同一 SQLite/
        // 附件目录（WAL 容许多连接，但收件/清理双线程互踩会互相平白发工作）。
        // 第二实例不进入 run，交出 argv 后立即退出
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
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
            // 启动降级（审计 P1-3）：自定义数据目录不可用（外置盘拔出/权限变更/
            // 库损坏）时回退锚点目录重试一次——迁移前的原数据就留在锚点，
            // 用户至少能进应用；两者皆失败才报错退出，且必须有可见提示
            let state = match state::AppState::try_open_with_anchor(&data_dir, &anchor) {
                Ok(s) => s,
                Err(err) if data_dir != anchor => {
                    eprintln!(
                        "[startup] 自定义数据目录不可用（{err}），回退默认目录 {}",
                        anchor.display()
                    );
                    match state::AppState::try_open_with_anchor(&anchor, &anchor) {
                        Ok(s) => s,
                        // 回退仍失败：错误信息里同时保留两条原因，便于定位
                        Err(err2) => {
                            return Err(startup_fatal(
                                app,
                                &format!("{err}\n\n回退默认目录同样失败：{err2}"),
                            ));
                        }
                    }
                }
                Err(err) => return Err(startup_fatal(app, &err)),
            };
            // M3 发送队列：单飞 worker（崩溃残留 queued 补发），窗口创建前就绪
            state.start_send_worker();
            app.manage(state);

            // M4 收件调度 + M5 清理器：常驻线程（每轮重读设置，改参数即时生效）
            spawn_fetch_scheduler(app.app_handle().clone());
            spawn_clean_scheduler(app.app_handle().clone());

            let url: tauri::Url = APP_URL.parse().expect("入口 URL 为常量，恒合法");
            // 主窗口在运行时创建：入口 URL 依赖平台（自定义协议映射差异），配置文件表达不了
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("CorpMail")
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
        .expect("CorpMail 运行失败");
}

/// 启动致命错误：原生对话框告知用户（GUI 子系统无控制台，stderr 无人可见），
/// 并把原因交回 setup 由 `run()` 统一终止。
fn startup_fatal(app: &tauri::App, detail: &str) -> Box<dyn std::error::Error> {
    use tauri_plugin_dialog::DialogExt;
    app.handle()
        .dialog()
        .message(format!(
            "CorpMail 无法启动。\n\n{detail}\n\n\
             请检查数据目录是否存在且可写；若曾修改数据目录，可删除配置文件 \
             corpmail.toml 中的 data_dir_override 后重新启动。"
        ))
        .title("CorpMail 启动失败")
        .blocking_show();
    detail.to_string().into()
}

/// 收件调度线程（§6.2）：启动 2s 首收（sync_on_start 默认开）+ sync_interval_min 循环。
/// 每轮重读设置（改参数即时生效；间隔 0 = 仅手动）。
fn spawn_fetch_scheduler<R: tauri::Runtime>(handle: tauri::AppHandle<R>) {
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
            let interval = state
                .get(state::keys::SYNC_INTERVAL_MIN)
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30);
            if interval > 0 {
                std::thread::sleep(std::time::Duration::from_secs(
                    interval.saturating_sub(1).saturating_mul(60),
                ));
                state.start_fetch();
            }
        }
    });
}

/// 存储清理器线程（§5.2）：启动 30s 首清（避开启动高峰）+ 每 24h 循环；
/// 策略每轮重读；失败仅记日志，下轮再试。
fn spawn_clean_scheduler<R: tauri::Runtime>(handle: tauri::AppHandle<R>) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(30));
        loop {
            let state = handle.state::<state::AppState>();
            if let Err(err) = clean::run(&state) {
                eprintln!("[clean] 清理器执行失败：{err}");
            }
            std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
        }
    });
}
