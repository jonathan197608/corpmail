# CorpMail · 企业邮箱客户端

CorpMail（中文名「企业邮箱客户端」）是一款面向中国电信企业邮箱的本地优先桌面客户端
（Tauri 2 + htmx，Rust 端渲染，无 Node 构建链）。全部邮件、附件与联系人只保存在本机，
不经过任何第三方服务器。

## 功能特性

- **收发一体**：IMAP 收信 + SMTP 发信，本地 SQLite 存储（FTS5 全文检索 + jieba 中文分词）
- **中文全文检索**：按相关度搜索标题与正文，命中内容高亮；支持中文/英文/编号混合、多关键词 AND 组合
- **传输加密可选**：465/993 默认隐式 TLS，587/143 自动 STARTTLS，可在设置显式指定（含仅可信内网的明文模式）；连接失败会指出"当前端口按哪种方式连的、该改成什么"
- **收件体积保护**：单封超过上限（默认 50MB）只拉信头并保存占位邮件，正文与附件不下载；分批入库受封数与累计字节双阈值约束
- **增量同步**：IMAP 增量同步，本地阅读后批量回写已读状态（1.5s 合并窗口，一次连接覆盖多封）；手动 / 定时自动收取，可调整首拉范围并按需重新拉取
- **撰写与回复**：草稿自动保存、回复 / 全部回复 / 转发、富文本编辑、多附件添加与内联预览（图片 / PDF / 文本）、发送失败一键重新编辑
- **导出**：单封导出 `.eml` / Markdown（Markdown 带 front matter 供 AI 分析，短附件内嵌、大附件给路径）、批量"全部另存"
- **联系人管理**：首拉自动采集往来发件人，支持搜索、新增、删除与收件人补全联想（仅计直接往来，群发抄送不计次）
- **邮件模板**：设置页维护，撰写页一键插入（含富文本模板）
- **存储管理**：数据目录可迁移，自动清理策略可调（过期邮件 / 大附件 / 孤儿文件），存储占用与可释放空间一目了然
- **数据延续**：首设向导可沿用既有数据目录（仅当账号一致时，历史邮件与收件进度直接继承，删除操作同样延续）
- **凭据安全**：密码保存于操作系统凭据保管（Windows 凭据管理器 / macOS 钥匙串），钥匙串不可用时降级为本地加密（AES-256-GCM，密钥独立存放）
- **发件显示名**：未填写时自动从发件箱的历史发件人推断并写回配置；用户主动清空后不再自动回填
- **桌面体验**：系统托盘常驻（关窗隐藏、未读角标、托盘菜单），单实例运行，纯净运行时零 Node 依赖

## 安装

### Windows（商店 / 直发双渠道）

- **Microsoft Store（MSIX）**：商店渠道分发，认证后可从商店获取；安装包由 Microsoft 商店签名
- **GitHub Release / 内网（NSIS）**：见 [Releases](https://github.com/jonathan197608/corpmail/releases)，运行 `CorpMail_x64-setup.exe` 默认安装到 `C:\Program Files\CorpMail`。该安装器**未做代码签名**，首次运行时 Windows SmartScreen 会提示「未知发布者」，需点「更多信息 → 仍要运行」

> 两种渠道共享同一数据目录（`%APPDATA%\com.corpmail.desktop`），数据互通；单实例互斥不跨渠道，请勿同时运行两个渠道实例。安装器内嵌 WebView2 官方引导器（静默模式），无 Runtime 的机器会自动安装。

首次启动按向导填写邮箱账号（默认服务器已按中国电信企业邮箱预填：IMAP `imap.chinatelecom.cn:993` / SMTP `smtp.chinatelecom.cn:465`）。数据保存目录**默认为空且必须由用户选择**，未选择无法继续；集团邮箱的两条 OA 指引（生成客户端专用密码、选择拉取范围）只在首次设置且域名匹配 `chinatelecom.cn` 时展示在页面顶部。

### 从源码构建

要求：Rust stable、macOS/Linux（交叉编译 Windows 产物需 mingw-w64）或 Windows 原生。

```bash
# macOS 交叉编译 Windows NSIS 安装器
# 运行时 crate `tauri` 为 2.12.5（见 Cargo.lock）；命令行 `tauri-cli` 独立发版，
# 当前最高为 2.12.4，两者版本号不同步属正常。
cargo install tauri-cli --version 2.12.4
cargo tauri build --target x86_64-pc-windows-gnu \
  --config '{"bundle":{"targets":["nsis"]}}'
```

> macOS 宿主构建 NSIS 需自建 patched makensis（macOS libiconv 对 "wchar_t" 转换通道存在 EILSEQ 回归，详见设计文档 §11.4——该问题在 Linux/Windows 宿主不存在）。Linux 宿主直接 `apt install nsis` 即可。

商店 MSIX 包由 `scripts/build-msix.ps1`（WinApp CLI）在 Windows 机上构建，详见 `store-assets/`。

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

[隐私声明（中文）](PRIVACY.zh-CN.md) · [Privacy Policy (English)](PRIVACY.en-US.md)：数据仅存储于本机，网络传输只发往您配置的 IMAP/SMTP 服务器，以及您打开的邮件正文里远程图片所在的服务器（应用仅代理取图，不带账号与正文）。

## 凭据存储

邮箱密码优先存系统钥匙串（Windows 凭据管理器 / macOS 钥匙串）。钥匙串不可用时
（权限受限、组策略禁用）降级为本地加密：密文存数据库 `settings` 表，**AES-256-GCM
密钥单独存 `<应用配置目录>/corpmail.key`（权限 0600）**，与数据库分开放置。
因此单独拿到 `corpmail.db`（备份、同步盘、误传）无法解出任何密码；密钥文件丢失
则需重新输入一次密码。

## Windows 发布物签名

- **Microsoft Store（MSIX）**：由 Microsoft 商店在认证与分发时签名，安装时发布者为商店登记主体。
- **GitHub Release / 内网（NSIS 直装包）**：**不签名**。项目此前评估过第三方免费云签名（SignPath Foundation），该路线已废弃并从构建链移除。直装包首次运行会出现 SmartScreen「未知发布者」提示，请核对 Release 页给出的 SHA-256 后再安装。
- macOS 侧见上文「macOS 本地开发」：本地自签名证书仅在开发机使用，不可用于分发。

## 许可证

[MIT](LICENSE)