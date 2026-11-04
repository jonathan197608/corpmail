//! 系统托盘（M4-1：常驻 + 未读角标）
//!
//! - 托盘图标常驻：关窗 = 隐藏主窗口（收件/定时发送等后台事务继续运行），
//!   真退出走托盘菜单「退出」
//! - 左键点击托盘 = 显示并聚焦主窗口；右键 = 菜单（显示主界面/立即收取/退出）
//! - 未读角标：macOS 托盘 title 显示未读数（菜单栏图标旁数字）；全平台
//!   tooltip 同步「N 封未读」。刷新时机：收件轮结束（notify 钩子）、
//!   UI 未读变更点（state.poke_tray）、发送调度线程每 20s 兜底

use crate::state::AppState;
use tauri::Manager;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

/// 托盘标识（tray_by_id 查找用）
pub const TRAY_ID: &str = "corpmail-tray";

/// 创建托盘（lib.rs setup 在 manage 之后调用一次；重复调用会因 id 冲突报错，
/// 单实例插件已保证进程唯一）
pub fn setup<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
    let fetch = MenuItem::with_id(app, "fetch", "立即收取新邮件", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出 CorpMail", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &fetch, &separator, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("CorpMail")
        .menu(&menu)
        // 左键留给「唤起主窗口」，菜单走右键（Gmail/Outlook 桌面版习惯）
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            "fetch" => app.state::<AppState>().start_fetch(),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    refresh_badge(app);
    Ok(())
}

/// 显示并聚焦主窗口（托盘左键 / 菜单 / 单实例回调共用；
/// 窗口曾被隐藏时 show 幂等恢复）
pub fn show_main<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 刷新未读角标（macOS title + 全平台 tooltip）。读 folder_counts 的
/// inbox_unread 计数——一次 SUM 查询，调用开销可忽略；托盘未创建时静默跳过
pub fn refresh_badge<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let unread = app
        .state::<AppState>()
        .folder_counts()
        .get("inbox_unread")
        .copied()
        .unwrap_or(0);
    // macOS：title 直接渲染在托盘图标旁（数字角标）；其余平台 title 不生效
    let title = if unread > 0 {
        Some(unread.to_string())
    } else {
        None
    };
    if let Err(err) = tray.set_title(title.as_deref()) {
        eprintln!("[tray] 未读角标更新失败（不影响主流程）：{err}");
    }
    let tip = if unread > 0 {
        format!("CorpMail — {unread} 封未读")
    } else {
        "CorpMail".to_string()
    };
    let _ = tray.set_tooltip(Some(tip.as_str()));
}
