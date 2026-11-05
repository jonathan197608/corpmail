# CorpMail macOS 功能自动化测试方案

结论先行：**用 WebdriverIO + `@wdio/tauri-service` 的 embedded 驱动，驱动真机 app 的
WKWebView 做元素级功能测试，每个功能点产出 1–3 张截图存证；外壳行为（托盘/关窗/
通知）用 AppleScript + `screencapture` 单独补一层。**

---

## 1. 为什么是这套（选型依据）

macOS 上的 Tauri GUI 自动化只有一条正路，原因是硬限制：

> Tauri 官方文档（2026-06-29 更新）：直接驱动 `tauri-driver` 时，桌面端只支持
> Windows 与 Linux，**macOS 没有可用的 WKWebView driver**；macOS 必须用
> `@wdio/tauri-service` 的 embedded provider（应用内嵌 W3C WebDriver 服务端）。

| 方案 | macOS | 元素级定位 | 截图 | 结论 |
|---|---|---|---|---|
| `tauri-driver` 直连 | ✗ | — | — | 官方明确不支持，排除 |
| **`@wdio/tauri-service` embedded** | ✓ | ✓ | ✓ | **主方案** |
| CrabNebula `tauri-driver` fork | ✓ | ✓ | ✓ | 需付费 key，备选 |
| Playwright + 测试专用 HTTP 端口 | ✓ | ✓ | ✓ | 见 §9 降级路径 |
| Appium mac2 | ✓ | ✗（WKWebView 内 DOM 不可见） | ✓ | 只适合外壳 |
| AppleScript + `screencapture` | ✓ | ✗（只能窗口级） | ✓ | 外壳层补充 |

本方案已经落地到仓库的 `e2e/` 目录，构建入口 `e2e/run.sh`。

## 2. 分层结构

```
L4 视觉回归（可选）  截图基线 + 像素/SSIM 阈值，防「功能对但样式崩」
L3 原生外壳层        scripts/e2e-shell-macos.sh：窗口存在、Cmd+W 隐藏、进程常驻、窗口截图
L2 GUI 功能层（主体）e2e/specs/*.spec.mjs：真实 app + 真实 WKWebView + mock 邮件服务器
L1 单元/集成层       cargo test --lib（158 项，已有，覆盖路由/解析/游标/凭据）
```

要点：L2 跑的是**打包前的真二进制**（`target/debug/corpmail`），不是浏览器里的 HTML
副本。页面通过自定义协议 `ui://localhost/` 加载，渲染、htmx 交互、轮询、磁盘库、
钥匙串全部是生产路径。

## 3. E2E 构建：一个 Cargo feature，不污染生产

`src-tauri/Cargo.toml`：

```toml
[features]
e2e = ["dep:tauri-plugin-wdio-webdriver"]

[dependencies]
tauri-plugin-wdio-webdriver = { version = "1", optional = true }
```

`src-tauri/src/lib.rs`（builder 链末）：

```rust
#[cfg(feature = "e2e")]
let builder = builder.plugin(tauri_plugin_wdio_webdriver::init());
```

- 只有 `cargo build --features e2e` 的产物会监听 `127.0.0.1:<TAURI_WEBDRIVER_PORT>`
- `cargo build` / `tauri build` 默认不带该 feature：**发布包无端口、无该插件**

## 4. 环境隔离（每次运行一个干净世界）

| 维度 | 做法 | 目的 |
|---|---|---|
| 用户目录 | `wdio.conf.mjs` 里把 `HOME` 指向 `e2e/artifacts/<run>/home` | 配置、库、日志、keyfile 全落 run 目录，不碰真实数据 |
| 邮件服务器 | `e2e/support/mockmail.py`，随机端口，明文 IMAP+SMTP | 内容可控、可断言、无外网依赖 |
| 端口 | `embeddedPort`（默认 4445，可用 `E2E_WD_PORT` 改） | 与日常实例隔离 |
| 单实例 | `run.sh` 先 `pkill -f target/debug/corpmail` | 否则 tauri-plugin-single-instance 会把新实例并到旧进程 |
| 凭据 | 每 run 新账号 `e2e@corpmail.test`；`run.sh` 用固定身份重签 | 避免 keychain ACL 随 cdhash 变化弹授权框卡住自动化 |

mock 服务器只实现客户端实际用到的命令（`recv.rs` / `send.rs` / `seen.rs`）：
`CAPABILITY / LOGIN / LIST / SELECT / UID SEARCH / UID FETCH / UID STORE / LOGOUT` +
SMTP `EHLO / AUTH PLAIN|LOGIN / MAIL / RCPT / DATA / QUIT`。
发出的信会落盘到 `artifacts/<run>/outbox/`，作为「真的发出去了」的直接证据。

为什么不用 Dovecot/Docker：客户端走 `security=plain`（`net::TlsMode::Plain`），
自签证书会被 lettre / imap 的 TLS 校验拒绝，改系统信任库不适合自动化；
而 mock 一个明文服务器只要 300 行，且能让每封邮件的内容完全确定。

## 5. 功能点矩阵与截图清单

每行 = 一个功能点，至少 1 张截图。**全部用例已实现**（`e2e/specs/`，★ = 首批 P0）。
spec 与数据依赖：`00→01` 依赖全新 HOME，`02–07` 依赖 01 完成向导与首拉，
必须按文件名顺序跑（wdio `maxInstances: 1` 顺序执行已保证）。

| # | 功能点 | 关键断言 | 截图 | spec |
|---|---|---|---|---|
| A1 ★ | 首启欢迎页 | 标题、三张特性卡、「开始配置」入口 | 2 | 00 |
| A2 ★ | 向导默认值 | SMTP 465 / IMAP 993 / 自动加密 | 1 | 01 |
| A3 ★ | 必填校验 | 空密码提交仍停留向导 | 1 | 01 |
| A4 ★ | 双通道测试并保存 | 跳转 `/welcome/done`；conncheck 打真 mock | 1 | 01 |
| A5 ★ | 完成页摘要 | 账号、服务器、凭据保管文案 | 1 | 01 |
| A6 ★ | 数据目录迁移（存储页签触发） | 「待生效目录」+ 重启徽标 + 当前目录注记 | 1 | 03 |
| B1 ★ | 首拉入库 | 列表出现 3 封 fixture 邮件 | 1 | 01 |
| B2 ★ | 未读/附件标记 | `.mail-row.unread`、`.mr-clip` | 1 | 01 |
| B3 ★ | 中文检索 | `q=季度` 命中 1 封 | 1 | 02 |
| B4 ★ | 清空检索 | 恢复全量列表 | 1 | 02 |
| B5 ★ | 只看未读 | toggle 后所有行 `.unread`；开关切回恢复 | 2 | 04 |
| B6 ★ | 只看外部 | 内部域过滤（mock 全内部 → 空态文案） | 1 | 04 |
| B7 ★ | 排序 | `date_asc` 首行=最早邮件；`subject_asc` 字典序 | 2 | 04 |
| B8 ★ | 每页条数 + 翻页 | `per=1` 单行 + 「下一页」；翻页显示第 2 封 | 2 | 04 |
| B9 ★ | 文件夹切换与侧栏计数 | sent/draft 空态文案；侧栏 `.unread-chip` 计数 | 2 | 04 |
| B10 ★ | 批量星标/取消 | 行选 → star → `.starred` 脊线；unstar 恢复 | 2 | 04 |
| B11 ★ | 批量删除 | 行数 -1，toast 确认文案 | 1 | 04 |
| B12 ★ | 高级检索：发件人 | `from=张伟` 命中欢迎邮件 1 封 | 1 | 04 |
| B13 ★ | 高级检索：仅看附件 | `att=1` 命中季度报表 1 封 | 1 | 04 |
| B14 ★ | 高级检索：日期区间 | `df/dt` 命中区间内邮件，区间外排除 | 1 | 04 |
| C1 ★ | 纯文本正文 | 正文文本完整、换行保留 | 1 | 02 |
| C2 ★ | 附件清单 | `report.csv` 出现、`.att-row` 数量正确 | 1 | 02 |
| C3 ★ | 附件预览模态 | `#preview-modal` 可见、关闭可用 | 1 | 02 |
| C4 ★ | 原文视图 | `/emails/<id>/source` 含 Message-ID/收发头 | 1 | 07 |
| C5 ★ | 导出 .eml（B4） | `htmx.ajax` POST dest → toast + 文件落盘 + 内容含头 | 2 | 07 |
| C6 ★ | 附件另存（单个/全部 A4） | toast「已保存 N 个附件」+ 落盘文件内容=fixture CSV | 2 | 07 |
| C7 | HTML 正文净化 | 无 `<script>`、外部图片走 `/proxy/image` | 1 | 07 |
| D1 ★ | 撰写页初始态 | 标题「撰写新邮件」、模板下拉为空占位 | 1 | 05 |
| D2 ★ | 草稿自动保存 | 输入后 `#autosave-hint` 出现「已自动保存」 | 1 | 05 |
| D3 ★ | 存草稿 | 草稿箱 +1，行出现在草稿箱列表 | 2 | 05 |
| D4 ★ | 立即发送 | `outbox/*.eml` 落盘（To/Subject 正确）+ 发件箱 +1 + 状态轮询「已发送」 | 3 | 05 |
| D5 ★ | 撤销发送（A1） | 定时发送 → toast 撤销按钮 → 回撰写页「已撤销发送」 | 2 | 05 |
| D6 ★ | 回复预填 | `reply_to` 收件人=原发件人、正文含引用 | 1 | 05 |
| D7 ★ | 回复全部 | 标题「回复全部」 | 1 | 05 |
| D8 ★ | 转发 | 主题转：前缀 +「将携带 1 个附件」提示 | 1 | 05 |
| D9 ★ | 附加 .eml 转发（B6） | 「将以附件 xxx.eml 加入」提示 | 1 | 05 |
| D10 ★ | 发送失败与重发 | mock 对 `fail@` 收件人 550 → failed 状态 + 重发按钮 → 重发回撰写页 | 3 | 90 |
| E1 ★ | 账号页签 | 当前账号、凭据保管状态 | 1 | 03 |
| E2 ★ | 同步页签保存回显 | 改间隔提交 → pane 回显新值 | 2 | 03 |
| E3 ★ | 连接测试按钮 | test-smtp / test-imap 结果区显示成功（打真 mock） | 2 | 03 |
| E4 ★ | 存储页签 | 目录、占用统计 | 1 | 03 |
| E5 ★ | 关于页签 | 版本与许可信息 | 1 | 03 |
| F1 ★ | 收件状态条 | 手动收取：进行中→空闲，无「凭据缺失/超时」 | 2 | 03 |
| F2 ★ | 联系人自动采集 | 3 位发件人出现且带姓名 | 1 | 06 |
| F3 ★ | 联系人搜索/新增/删除 | 过滤 1 行；新增出现；删除消失 | 3 | 06 |
| F4 ★ | 收件人补全接口 | `suggest?q=` JSON 含地址（页面 fetch 注入断言） | 1 | 06 |
| F5 ★ | 邮件模板增删 | 保存后列表出现；撰写页 `#tpl-select` 有该项；删除消失 | 3 | 06 |
| G1 | 关窗隐藏（外壳） | Cmd+W 后进程常驻、窗口数 0 | 1 | L3 |
| G2 | 托盘唤回（外壳） | 点击 Dock/托盘后窗口重新出现 | 1 | L3 |

### 失败链路（独立 spec：`90-failure.spec.mjs`，最后执行）

成功链路（00–07）不注入任何失败；失败注入全部集中在 90，手段外部可控
（不改应用配置、不依赖 mock 停机）：

| # | 失败场景 | 注入方式 | 关键断言 |
|---|---|---|---|
| H1 | 向导空字段提交 | htmx.ajax POST 空表单 | 「请填写邮箱账号与密码」+ 无 HX-Redirect |
| H2 | 向导双通道失败 | SMTP/IMAP 指向 127.0.0.1:1 | 「未保存」+ 无跳转 + **已有配置不被覆盖**（列表仍可用） |
| H3a | 迁移：空目标 | dest 为空 | 「未选择目标目录」 |
| H3b | 迁移：同当前目录 | dest = pane 当前目录 | 「目标目录与当前数据目录相同」 |
| H3c | 迁移：目标已有库 | dest 预置假 corpmail.db | 错误含 corpmail.db + 当前目录不变 |
| H4 | 发送失败状态机 | mock 拒收 fail@（550） | 撤销窗后详情页 `.md-status.failed` + 重发按钮 → 回撰写页预填 |

### 分层原则

- **00–07（成功链路）**：先保证正向全绿；失败注入出现其中会让后续断言失去
  前提（配置、首拉、草稿计数），且 `bail: 1` 会在第一个失败处停下
- **90（失败链路）**：每个用例注入失败后验证「拒绝 + 状态可恢复」，
  H2 还顺带验证失败不产生副作用
- **L1**：队列状态机自愈（sending 被杀复位 failed）、非 failed 重发被拒等
  纯状态机分支仍归单测；L3：关窗/托盘外壳行为

## 6. 截图规范

- **目录**：`e2e/artifacts/<ISO 时间戳>/`，每轮一个目录，永不覆盖历史
- **命名**：`NNN-<语义名>.png`，`NNN` 全局递增（顺序即操作顺序，便于复盘）；中文语义名
  直接对应功能点，如 `003-向导-默认配置.png`
- **失败现场**：用例失败时 `afterTest` 自动补一张 `FAIL-<标题>.png`
- **索引**：每轮生成 `index.md`（截图清单，按序号排列，可直接点开），以及 `shots.json`
- **隔离**：`home/`（隔离用户目录）、`outbox/`（mock 收到的信）、日志同目录，随 run 归档
- **内容要求**：截图必须包含**可判读的状态**（过滤条件、计数、时间、错误文案），
  不要只截空页面；同一功能点若含「操作前/后」两张，必须成对出现

## 7. 运行方式

```sh
# 全量（构建 → 重签 → 清理残留 → mock + 4 个 spec）
e2e/run.sh

# 单点调试
e2e/run.sh --spec specs/01-wizard.spec.mjs

# 换端口（与日常实例并存）
E2E_WD_PORT=4455 e2e/run.sh

# 只跑外壳层（需先授权辅助功能 + 屏幕录制）
scripts/e2e-shell-macos.sh
```

首轮会看到：`[E2E] mock 邮件服务器 imap=xxxxx smtp=xxxxx，初始邮件 3 封`，
结束后输出截图目录。`e2e/artifacts/<run>/index.md` 是给人工复核的入口。

新增一个功能点用例（三步）：在 `specs/` 新建 `NN-<模块>.spec.mjs` → 用
`goto()` 到位、`$()`/`$$()` 断言 → 关键画面调 `shot('<功能点>')`。截图清单自动汇总。

## 8. 已知边界

| 覆盖不到 | 原因 | 替代 |
|---|---|---|
| 托盘菜单项点击 | 不在 WebView 内 | L3：AppleScript 点状态栏项（需辅助功能授权） |
| 系统通知横幅 | OS 层 UI，无法断言 | 只断言进程内通知调用（后端日志），不截横幅 |
| 文件选择/另存对话框 | 原生 NSOpenPanel，WebDriver 不可见 | L3：AppleScript 驱动，或用表单直填路径绕开 |
| 真实企业服务器行为 | 需要真实账号与网络 | 用 mock 覆盖协议分支；真实环境只做人工冒烟 |
| 窗口缩放/DPI | WebDriver 只保证 DOM 视口 | L3 `screencapture` + 人工抽检 |
| 服务焦点管理 | `@wdio/tauri-service` 每次 $/点击前调 `plugin:wdio|get_window_states`，需要配套前端插件 `@wdio/tauri-plugin` + Rust 插件 `tauri-plugin-wdio`（注意与 `tauri-plugin-wdio-webdriver` 是**两个 crate**）+ `withGlobalTauri`，缺任何一环每条查询白等 5s 超时（WARN `Tauri core.invoke not available`） | `suppressFocusManager()`：before 钩子里显式 `switchToWindow` 一次，服务的 auto-focus 即永久静默（官方设计）。单窗口应用无损失；将来要用 `browser.tauri.mock()` 再补全插件栈 |

## 9. 降级路径（若 embedded 驱动不可用）

给测试构建加一个本地 HTTP 出口（`#[cfg(feature = "e2e")]` 下 `tiny_http` 绑定
`127.0.0.1:0`，复用 `ui::route`），再用 Playwright(Chromium) 驱动同一套模板与后端。
优点：截图/断言稳定、无需 GUI 会话；缺点：不是真 WKWebView、不覆盖 Tauri 外壳，
且需要为测试多开一个端口。仅在 embedded 路线受阻时启用。

## 10. 实施计划

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0（已完成） | 构建 feature、隔离环境、mock 服务器、L2 全量用例（00–07 共 51 个 it） | `e2e/run.sh` 全绿，截图 ≈60 张 |
| P1 | 跑全量修选择器偏差（批量/模板/迁移的 DOM 细节以实测为准） | 全量 spec 通过 |
| P2 | L3 外壳层接入 CI、L4 视觉基线（pixelmatch 阈值 0.1%） | 关键页截图 diff 稳定 |
| P3 | GitHub Actions `macos-14` 跑全量（app 需 GUI 会话，先验证 runner 能力） | 每次 push 出截图 artifact |

## 11. 当前实测状态（不夸大）

已在本机验证：

- `cargo build --features e2e` 成功，产物启动后 **`127.0.0.1:4445` 返回
  `{"value":{"message":"waiting for webview initialization","ready":false}}`**
  ——内嵌 WebDriver 服务端确实起来了
- `mockmail.py` 用 `imaplib` + `smtplib` 全命令自测通过（SEARCH/FETCH/STORE/DATA）
- 隔离 `HOME` 生效：数据库落在 `artifacts/<run>/home/Library/Application Support/com.corpmail.desktop/`
- **smoke spec 全链路通过**（会话/元素查询/`saveScreenshot`），焦点管理 WARN 已静默
- 00–07 共 51 个用例全部编写完成并通过语法校验；选择器均核对过模板源码

尚未验证（首次全量跑时确认）：

- `browser.execute`（WebDriver ExecuteScript，经插件 eval_channel）：
  `autoConfirm`/`htmxxAjax`/suggest 断言依赖它，若驱动未实现会在
  04-B11、03-A6、06-F4、07-C5/C6/C7 明确报错
- 全量 spec 的实际通过情况与 ≈60 张截图产出

首次运行建议：`e2e/run.sh --spec specs/00-smoke.spec.mjs` 先确认会话与截图链路，
再跑全量。若 `saveScreenshot` 返回空或报 not implemented，L3 的 `screencapture`
可作为截图兜底（方案 §9 的降级路径同样适用于截图）。

不想跑整套 wdio 时，用诊断脚本三秒定位是哪一环断了：

```sh
python3 e2e/support/probe_wd.py \
  --bin src-tauri/target/debug/corpmail \
  --home e2e/artifacts/diag-home \
  --out e2e/artifacts/diag.png
```

它依次打印 `/status` → `/session` → 元素查询 → 截图字节数，失败点即是断点。
