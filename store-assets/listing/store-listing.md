# CorpMail · Microsoft Store 上架文案（Partner Center 逐字段对照表）

> 字段限制与提交位置均按 Partner Center「应用一览（Store listing）」页。
> 中英双语各一份；除标注「两者通用」外，均分别填写。

---

## 1. 应用名称（App name）

- **zh-CN**：`CorpMail 企业邮箱客户端`
- **en-US**：`CorpMail Business Mail Client`

（Partner Center 保留名：JonathanHuang.corpmail；商店显示名可不同于保留名）

## 2. 提交说明（Notes to certification，两者通用，英文提交）

```
CorpMail is a native email client for business mailboxes (SMTP/IMAP).
All data stays on the user's PC: mail is stored in a local SQLite database,
account passwords are stored in Windows Credential Manager. The app connects
directly to the user's own mail servers; we operate no servers and collect
no data. No account creation is required within the app; users configure
their existing email accounts on first launch.

Test note: On first launch the app shows a setup wizard requiring an SMTP/IMAP
account. Any standard account can be used for testing. Without configuring,
the wizard page is the only reachable screen (by design).
```

（认证备注至关重要：审核员没有邮箱账号可测，明确说明首次启动为配置向导，避免「功能无法验证」类驳回）

## 3. 说明（Description）

### zh-CN（≤ 10000 字符）

```
CorpMail 是一款为办公场景打造的本地优先邮箱客户端：收发一体、全部邮件本地存档、中文全文检索，数据不出本机。

【为什么选择 CorpMail】

■ 隐私优先，数据不出本机
全部邮件保存在本机数据库，不经过任何第三方服务器；账号密码存入 Windows 凭据管理器，数据库不留明文。我们不运营任何服务器，也不收集任何数据。

■ 标准协议直连自有邮箱
发信走 SMTP、收信走 IMAP，直连你自己公司的邮件服务器。无需在应用内注册新账号，首次启动填入现有邮箱即可使用。

■ 中文全文检索快人一步
基于中文分词的全文索引，输入关键词即可按相关度搜索标题与正文，命中内容高亮显示。支持中文、英文、编号混合搜索，多关键词 AND 组合。

■ 全部邮件本地存档
附件自动压缩存储，大附件分级管理；已发送邮件与大附件可按策略自动清理，也可一键手动清理，邮箱数据尽在掌控。

■ 精致流畅的桌面体验
原生窗口 + 轻量界面，启动即用；收取新邮件、发送、清理全部后台执行不打断浏览；撰写、回复、草稿、附件一应俱全。

■ 像素风图标 · 一眼可辨
全新像素风设计语言，蓝白撞色，清爽有记忆点。

【适合谁用】

· 使用公司企业邮箱（支持自定义服务器地址）的办公人群
· 需要长期保留邮件往来记录、随时检索历史邮件的岗位
· 关心邮件数据隐私，希望邮件只存在自己电脑上的用户

【功能一览】

· 收件（IMAP）：手动 / 定时自动收取，未读标记，已读状态回写服务器
· 发件（SMTP）：异步发送队列，失败可一键重新编辑发送
· 撰写与回复：草稿自动留存，多附件添加、在线预览（图片 / PDF / 文本）、另存为
· 全文检索：中文分词 + 相关度排序 + 关键词高亮
· 存储管理：数据目录可迁移，自动清理策略可调，存储占用一目了然
· 单实例运行：重复启动自动聚焦已有窗口

【隐私与安全】

CorpMail 不内置统计分析、不联网上报任何信息；唯一网络行为是与你配置的邮件服务器通信。密码仅存于 Windows 凭据管理器，数据库中不落明文。

【支持】

问题反馈与源代码（MIT 开源）：https://github.com/jonathan197608/mailbox
```

### en-US（≤ 10000 字符）

```
CorpMail is a local-first email client for business mailboxes: send and receive in one app, full local archive of every message, and Chinese full-text search — your data never leaves your PC.

【Why CorpMail】

■ Privacy first — everything stays local
All mail is stored in a local database and never passes through any third-party server. Account passwords live in Windows Credential Manager, never in plaintext. We run no servers and collect nothing.

■ Standard protocols, your own mailbox
Send via SMTP, receive via IMAP — direct connections to your company's mail servers. No new account needed inside the app; just configure your existing mailbox on first launch.

■ Chinese full-text search
Tokenized full-text index with relevance ranking across subjects and bodies, keyword highlighting, Chinese/English/mixed queries, and multi-keyword AND combinations.

■ Complete local archive
Attachments are gzip-compressed and tiered by size; sent mail and large attachments can be auto-cleaned by policy or on demand. Your mailbox footprint is always in view.

■ A fast, polished desktop experience
Native window, instant start. Fetching, sending and cleaning all run in the background without interrupting browsing. Compose, reply, drafts and attachments included.

■ Pixel-art icon, instantly recognizable
A crisp blue-and-white pixel design language.

【Who it's for】

· Office workers using company email (custom server addresses supported)
· Roles that need long-term mail archives and instant history search
· Anyone who wants their mail stored on their own machine only

【Feature list】

· Receive (IMAP): manual / scheduled fetch, unread flags, read-state sync back to server
· Send (SMTP): asynchronous queue with one-click edit-and-resend on failure
· Compose & reply: auto-saved drafts, multi-attachment add, inline preview (images / PDF / text), save-as
· Search: tokenized FTS with relevance ranking and highlighting
· Storage: movable data directory, tunable auto-clean policies, size at a glance
· Single instance: relaunching focuses the existing window

【Privacy & security】

CorpMail embeds no analytics and phones nothing home; its only network activity is talking to the mail servers you configure. Passwords are stored exclusively in Windows Credential Manager.

【Support】

Issues & source code (MIT): https://github.com/jonathan197608/mailbox
```

## 4. 搜索关键词（Search terms，7 词上限，≤ 30 字符/词，两者各自填写）

- **zh-CN**：`邮箱` `邮件` `企业邮箱` `收件` `办公` `邮件客户端` `FTS 搜索`
- **en-US**：`mail` `email` `client` `smtp` `imap` `offline` `archive`

## 5. 此版本的新增功能（What's new，两者通用模板）

```
v0.2.4
· Microsoft Store 首发版本
· 收发一体（SMTP/IMAP）、草稿与附件、中文全文检索、存储管理
```

## 6. 类别（Category）

- 一级：`Productivity（效率）`
- 二级：`Email` 或留空（如选项存在）

## 7. 年龄分级（Age ratings）

全部选「否」：不含 violence/nudity/gambling 等 → 分级预期 **3+**

## 8. 应用功能声明（App declarations）

- 程序包密码保护：否
- 此应用是否收集任何信息（隐私政策相关问答）：**否——应用不收集、不上报任何个人信息；仅与你配置的邮件服务器通信**

## 9. 定价（Pricing）

免费（后续如变 more，另行决策）

## 10. 隐私政策 URL（Privacy policy URL）

- URL：`https://github.com/jonathan197608/mailbox/blob/main/PRIVACY.zh-CN.md`
- **注意**：隐私文档已随仓库根目录公开（`PRIVACY.zh-CN.md`/`PRIVACY.en-US.md`），链接推送即生效。

## 11. 支持联系方式（Support contact info）

- 电子邮件：`18988991633@189.cn`
- 网站：`https://github.com/jonathan197608/mailbox/issues`

## 12. 商店徽标（Store logos）

| 文件 | 规格 | 状态 |
|---|---|---|
| `Square44x44Logo.png` 等 | 见 msix/assets | ✅ 已生成 |
|商店大图 1080×1080 / 1920×1080（Hero）| Partner Center 首页横幅 | ⏳ 可用截图工具拼装（见截图步骤）|

## 13. 截图清单（Screenshots 见 store-assets/screenshots/）

商店要求 ≥1 张截图，≥1366×768，PNG/JPG。推荐 1920×1080 × 5 张：

1. 主界面：收件箱列表（含侧栏 + 未读标记 + 搜索框）
2. 邮件详情：含附件预览弹层
3. 撰写页：多附件 + 已保存附件清单
4. 全文检索结果：关键词高亮命中
5. 设置·存储管理：存储占用 + 清理预估

## 14. 提交流程（两渠道关系声明文档引用）

提交前阅读 `store-assets/msix/README.md`（渠道共存说明：NSIS 与 Store 渠道可双装，单实例互斥不跨渠道）。
