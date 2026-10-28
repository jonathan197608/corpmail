// Mailbox - Tauri + htmx 桌面应用（Rust 端渲染 HTML，无 Node 构建链）
//
// GUI 程序：release 模式下隐藏 Windows 控制台窗口（debug 保留控制台便于看日志）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // WebView2 Runtime 的安装由官方 NSIS 安装器负责
    // （webviewInstallMode = embedBootstrapper，设计 §11.2 v3.4）；
    // 主程序不做启动自愈，也不携带 Bootstrapper。
    mailbox_lib::run();
}
