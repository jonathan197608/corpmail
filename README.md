# CorpMail

CorpMail 是一款面向中国电信企业邮箱的桌面客户端（Tauri 2 + htmx，Rust 端渲染，无 Node 构建链）。

## 功能特性

- **收发一体**：IMAP 收信 + SMTP 发信，本地 SQLite 存储（FTS5 全文检索 + jieba 中文分词）
- **传输加密可选**：465/993 默认隐式 TLS，587/143 自动 STARTTLS，也可在设置里显式指定（含仅可信内网的明文模式）；连接失败会指出"当前端口按哪种方式连的、该改成什么"
- **收件体积保护**：单封超过上限（默认 50MB）只拉信头并保存占位邮件，正文与附件不下载；分批入库同时受封数与累计字节双阈值约束
- **凭据安全**：密码保存于操作系统凭据保管（Windows 凭据管理器 / macOS 钥匙串），本地加密回退
- **增量同步**：IMAP 增量同步，本地阅读后批量回写已读状态（1.5s 合并窗口，一次连接覆盖多封）
- **纯净运行时**：零 Node 依赖，原生性能，Windows 单机安装（NSIS 安装器）

## 快速开始

### Windows 安装

1. 从 [Releases](https://github.com/jonathan197608/corpmail/releases) 下载 `CorpMail_x64-setup.exe`
2. 运行安装（默认安装到 `C:\Program Files\CorpMail`）
3. 首次启动按向导填写邮箱账号（默认服务器已按中国电信企业邮箱预填：IMAP `imap.chinatelecom.cn:993` / SMTP `smtp.chinatelecom.cn:465`）

> 安装器内嵌 WebView2 官方引导器（静默模式），无 WebView2 Runtime 的机器会自动安装（约 150MB，需联网）。

### 从源码构建

要求：Rust stable、macOS/Linux（交叉编译 Windows 产物需 mingw-w64）或 Windows 原生。

```bash
# macOS 交叉编译 Windows NSIS 安装器
cargo install tauri-cli --version 2.12.4
cargo tauri build --target x86_64-pc-windows-gnu \
  --config '{"bundle":{"targets":["nsis"]}}'
```

> macOS 宿主构建 NSIS 需自建 patched makensis（macOS libiconv 对 "wchar_t" 转换通道存在 EILSEQ 回归，详见设计文档 §11.4——该问题在 Linux/Windows 宿主不存在）。Linux 宿主直接 `apt install nsis` 即可。

### macOS 本地开发

首次需创建本地代码签名证书（每台机器一次）：

```bash
scripts/setup-macos-signing.sh   # 建自签名证书到登录钥匙串
scripts/dev-macos.sh             # 构建 → 重签 → 启动
```

不要直接用 `cargo tauri dev` 或 `cargo run`：链接器会给产物打 ad-hoc 签名，其 designated requirement 是 cdhash，每次重新链接都变；钥匙串条目的 ACL 按 DR 校验，于是每次构建后首次读密码都会弹授权窗口，点「始终允许」也只对当次 cdhash 生效。`dev-macos.sh` 在构建后用固定证书重签，DR 变为 `identifier + certificate leaf`，跨构建恒定。

`tauri build` 打包时使用 `tauri.conf.json` 的 `bundle.macOS.signingIdentity`（默认 `CorpMail Local`）；需要临时换证书时用环境变量 `APPLE_SIGNING_IDENTITY` 覆盖。该证书**不在信任链内**，仅供本地签名，分发请改用 Developer ID 并公证。


## 技术栈

| 组件 | 选型 |
|---|---|
| 桌面框架 | Tauri 2（Rust 后端 + 系统 WebView） |
| 前端 | htmx + 服务端渲染（minijinja 模板） |
| 存储 | SQLite（rusqlite bundled，FTS5 + jieba 预分词） |
| 协议 | IMAP 3.0（收件）、lettre/SMTP（发件） |

## 隐私声明

[隐私声明](PRIVACY.md)：数据仅存储于本机，网络传输仅发往您配置的 IMAP/SMTP 服务器。

## 凭据存储

邮箱密码优先存系统钥匙串（Windows 凭据管理器 / macOS 钥匙串）。钥匙串不可用时
（权限受限、组策略禁用）降级为本地加密：密文存数据库 `settings` 表，**AES-256-GCM
密钥单独存 `<应用配置目录>/corpmail.key`（权限 0600）**，与数据库分开放置。
因此单独拿到 `corpmail.db`（备份、同步盘、误传）无法解出任何密码；密钥文件丢失
则需重新输入一次密码。

## 代码签名

本项目的 Windows 发布物由 SignPath Foundation 免费提供代码签名，详见 [代码签名政策](CODE_SIGNING.md)。

## 许可证

[MIT](LICENSE)