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
mod notify;
mod secrets;
mod state;
mod templates;
mod tray;
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
    let builder = tauri::Builder::default()
        // 单实例（首个注册，确保在其他插件初始化前拦截第二实例；
        // 重复启动时聚焦已有主窗口而非新开，避免双实例并发写同一 SQLite/
        // 附件目录（WAL 容许多连接，但收件/清理双线程互踩会互相平白发工作）。
        // 第二实例不进入 run，交出 argv 后立即退出
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 唤起聚焦与托盘左键/菜单同一实现（tray::show_main：
            // unminimize + show + set_focus，窗口曾被隐藏时 show 幂等恢复）
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
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
            // 新邮件通知钩子（收件轮完成 → 本轮窗口内未读新件汇总一条系统通知）
            // notify 自建独立连接查询（WAL 下与主库读写不互阻，收件线程回调零锁竞争）。
            // db_path 直接取自手上的 state（勿用 app.state()：本注册点在 manage() 之前，
            // state() 会 panic "called before manage()"）
            {
                let handle: tauri::AppHandle = app.handle().clone();
                let db_path = state.data_dir().join("corpmail.db");
                let notify_handle = handle.clone();
                state.set_notify_hook(std::sync::Arc::new(move |since, _added| {
                    notify::on_fetch_finished(&notify_handle, &db_path, since);
                    // 收件轮结束未读数必已变化：同步刷新托盘角标
                    tray::refresh_badge(&notify_handle);
                }));
                // 托盘角标刷新钩子（UI 未读变更点 poke；调度线程兑底重刷）
                state.set_tray_hook(std::sync::Arc::new(move || {
                    tray::refresh_badge(&handle);
                }));
            }
            app.manage(state);

            // M4-1 托盘常驻：图标 + 菜单 + 左键唤起 + 未读角标（setup 需在
            // manage 之后：refresh_badge 读 AppState）
            tray::setup(app.handle())?;

            // M4 收件调度 + M5 清理器 + M4-2 定时发送调度：常驻线程
            // （每轮重读设置，改参数即时生效）
            spawn_fetch_scheduler(app.app_handle().clone());
            spawn_clean_scheduler(app.app_handle().clone());
            spawn_send_scheduler(app.app_handle().clone());
            // 已读回写 worker：合并窗口 + 单次连接批量 STORE（替代每封一次登录）
            spawn_seen_worker(app.app_handle().clone());

            let url: tauri::Url = APP_URL.parse().expect("入口 URL 为常量，恒合法");
            // 主窗口在运行时创建：入口 URL 依赖平台（自定义协议映射差异），配置文件表达不了
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("企业邮箱客户端")
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
        // M4-1 托盘常驻：关窗 = 隐藏主窗口（后台收件/定时发送继续），
        // 真退出走托盘菜单「退出」/ Cmd+Q（applicationShouldTerminate 不经
        // 窗口 CloseRequested，不受 prevent_close 影响）。Windows 无 Dock，
        // 首次隐藏时发一条引导通知（仅一次，避免打扰）
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                #[cfg(windows)]
                {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static TIPPED: AtomicBool = AtomicBool::new(false);
                    if !TIPPED.swap(true, Ordering::Relaxed) {
                        use tauri::Manager;
                        use tauri_plugin_notification::NotificationExt;
                        let app = window.app_handle().clone();
                        let _ = app
                            .notification()
                            .builder()
                            .title("CorpMail 仍在运行")
                            .body("主窗口已隐藏到系统托盘，点击托盘图标可重新打开。")
                            .show();
                    }
                }
            }
        });

    // E2E 专用：内嵌 W3C WebDriver 服务端（仅 `--features e2e` 构建）。
    // macOS 没有 WKWebView 的外部 driver（tauri-driver 只支持 Windows/Linux），
    // 插件在进程内起 HTTP 端口，由 @wdio/tauri-service 直连驱动 DOM 与截图。
    // 生产构建不编入该插件（Cargo [features] 默认关闭），无端口、无额外攻击面。
    #[cfg(feature = "e2e")]
    let builder = builder.plugin(tauri_plugin_wdio_webdriver::init());

    // 替代 builder.run(ctx)（其回调为空，无法订阅事件）：build + run 显式
    // 事件循环，订阅 macOS Reopen。行为与原 .run 一致（Tauri 的 run 内部
    // 即 build + run(空回调)）
    let app = builder
        .build(tauri::generate_context!())
        .expect("CorpMail 构建失败");

    // macOS 点击 Dock 图标 / 系统通知（userReactivated）→ applicationShouldHandleReopen。
    // 通知点击时系统只激活进程，最小化/被遮挡的主窗口不会自动前置——统一
    // 唤起聚焦（M1-4，与单实例插件聚焦同模式）。Windows 无对应事件：打包版
    // toast 点击自带前台激活（AppUserModelID 匹配），dev 下点击经单实例
    // 插件聚焦，均到达同样效果，无需代码路径
    // （Windows 侧零改动，无回归风险；真机行为随下次 Windows 发版联动验证）
    app.run(|_app_handle, _event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event
            && let Some(win) = _app_handle.get_webview_window("main")
        {
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    });
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

/// 已读回写 worker（审计批 3）：单线程 + 1.5s 合并窗口。
///
/// 旧实现每点开一封未读邮件就 spawn 一个线程做完整 IMAP 连接/登录/SELECT/STORE，
/// 连点 20 封即 20 次登录（企业服务器按登录频次限流，触发后长时间收不到信）。
/// 现在：本地置已读只写库+唤醒；worker 窗口内合并，一次连接、单条命令覆盖多 UID。
/// 启动时自唤醒一次：补发上次崩溃遗留的 seen_pending 标志。
fn spawn_seen_worker<R: tauri::Runtime>(handle: tauri::AppHandle<R>) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    handle.state::<state::AppState>().set_seen_channel(tx);
    let worker_handle = handle.clone();
    std::thread::spawn(move || {
        let mut failures = 0u32;
        loop {
            // failures == 0：阻塞等待唤醒（空闲零开销；channel 关闭说明状态已销毁），
            // 醒来后合并窗口把连点合成一批；failures > 0：上一轮失败且退避已睡完，
            // 直接重试（旧实现睡完回 recv 阻塞——退避被架空成"等用户再点一封"，
            // 瞬时网络抖动后必须等用户下一次点击才补得上）
            if failures == 0 {
                if rx.recv().is_err() {
                    return;
                }
                std::thread::sleep(mail::seen::MERGE_WINDOW);
                while rx.try_recv().is_ok() {}
            }
            let state = worker_handle.state::<state::AppState>();
            match mail::seen::flush(&state, &mail::RealFetcher) {
                Ok(n) => {
                    if n > 0 {
                        eprintln!("[seen] 已回写 {n} 封的服务器已读状态");
                    }
                    failures = 0;
                }
                Err(err) => {
                    failures += 1;
                    eprintln!("[seen] 回写失败（第 {failures} 次）：{err}");
                    if failures >= mail::seen::MAX_ATTEMPTS {
                        // 连续失败达上限：清标志放弃，避免永久重试（本地已读不受影响）
                        mail::seen::abandon(&state);
                        failures = 0;
                        continue;
                    }
                    std::thread::sleep(mail::seen::backoff(failures));
                }
            }
        }
    });
    // 崩溃残留补发（上次进程在标志置位后退出，server 侧仍显示未读）
    handle.state::<state::AppState>().notify_seen();
}

/// 收件调度线程（§6.2）：启动 2s 首收（sync_on_start 默认开）+ sync_interval_min 循环。
/// 每轮重读设置；间隔 0 = 仅手动。
///
/// 等待期按 30s 分片：旧实现一次 `sleep(整个周期)`，改间隔最长要等满一个旧周期才生效
/// （注释称"即时生效"不成立）。分片后设置改动最多 30s 生效（审计 P2-17）。
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
        // 间隔读取（秒；0 = 仅手动）
        let read_interval = |handle: &tauri::AppHandle<R>| -> u64 {
            handle
                .state::<state::AppState>()
                .get(state::keys::SYNC_INTERVAL_MIN)
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30)
        };
        loop {
            let mut interval = read_interval(&handle);
            if interval == 0 {
                // 仅手动：小睡后重读，不触发（也避免忙等）
                std::thread::sleep(std::time::Duration::from_secs(30));
                continue;
            }
            let mut waited = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                waited += 30;
                let now = read_interval(&handle);
                if now != interval {
                    interval = now;
                    if interval == 0 {
                        break;
                    }
                }
                if waited >= interval.saturating_mul(60) {
                    break;
                }
            }
            if interval > 0 {
                let st = handle.state::<state::AppState>();
                // 后台更新进行中：本次定时触发直接忽略（不排队、不叠加线程）。
                // start_fetch 内部 CAS 亦兜底，这里提前短路主要是语义明确 + 少一次调用
                if st.fetch_snapshot().running {
                    eprintln!("[fetch] 定时触发时已有收件进行中，忽略本次");
                } else {
                    st.start_fetch();
                }
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

/// 定时发送调度线程（M4-2 + A1）：每 2s 扫描到点草稿并派发。
/// A1 撤销窗口只有 8 秒，扫描间隔必须降到秒级（否则窗口被扫描周期吞掉）；
/// 查询走 idx_scheduled_due 部分索引，2s 一次的代价可忽略。
/// 启动即扫一轮——应用关闭期间到点的定时邮件，下次启动立即补发。
/// 同一轮兼作托盘角标兑底刷新：每 10 轮（20s）一次，保持原有节奏
fn spawn_send_scheduler<R: tauri::Runtime>(handle: tauri::AppHandle<R>) {
    std::thread::spawn(move || {
        let dispatched = mail::dispatch_due_scheduled(&handle.state::<state::AppState>());
        if dispatched > 0 {
            eprintln!("[sched] 启动补发定时邮件 {dispatched} 封");
        }
        let mut tick = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let state = handle.state::<state::AppState>();
            mail::dispatch_due_scheduled(&state);
            tick += 1;
            if tick.is_multiple_of(10) {
                tray::refresh_badge(&handle);
            }
        }
    });
}
