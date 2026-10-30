# CorpMail · Microsoft Store 上架资产包

本目录是商店上架的**配置、文案与截图**资产（MSIX 包体由 `scripts/build-msix.ps1`（PowerShell）在 Windows 上构建）。

## 目录结构

```
store-assets/
├── msix/
│   ├── AppxManifest.xml     # 商店包清单（Identity.Name=corpmail 已定稿；Publisher 为占位）
│   ├── assets/              # 全套商店图标（像素风，最近邻放大无损）
│   └── （out/ 构建产物，不入库）
├── listing/
│   ├── store-listing.md     # Partner Center 逐字段对照表（中英双语）
│   ├── PRIVACY.zh-CN.md     # 隐私声明草稿（正式版已发布在仓库根 PRIVACY.zh-CN.md）
│   └── PRIVACY.en-US.md
└── screenshots/             # 1920×1080 商店截图 ×5（真实页面渲染）
```

## 上架身份（已定稿）

- **Publisher**：`CN=3DAC9C91-AE75-4F8D-A413-16DBAF29757A`（已写入 manifest）
- **隐私政策 URL**：`https://github.com/jonathan197608/corpmail/blob/main/PRIVACY.zh-CN.md`
 （隐私文档已随仓库根目录公开）
- 内部文档（设计稿等）位于 `docs-private/`，不公开（.gitignore 拦截）

## 渠道共存说明（重要，写进提交备注已备）

- **NSIS 渠道**（官网/GitHub 直发）与 **Store 渠道**（MSIX）可同时装在一台机器上——
  两者身份标识不同，互不覆盖。
- AppData 虚拟化已声明禁用，两渠道读写**同一个** `%APPDATA%\com.corpmail.desktop` 数据目录：
  - 从 NSIS 切 Store：邮件/配置/密码原样继承
  - 但两渠道**同时运行**时会双进程写同一 SQLite——
    单实例互斥体（tauri-plugin-single-instance）**不跨渠道生效**，
    请勿双渠道同时开应用；日常使用建议二选一。
- Server 2016 无商店，公司内网验收环境固定走 NSIS 渠道，不受影响。

## 截图清单（提交到 Partner Center「截图」区）

| 文件 | 场景 |
|---|---|
| `01-main-inbox.png` | 收件箱（6 封真实感种子：未读点/附件夹/近期时间） |
| `02-detail-attachments.png` | 邮件详情 + 附件区 |
| `03-compose.png` | 撰写（含已填示例内容） |
| `04-search.png` | 全文检索高亮命中 |
| `05-storage.png` | 存储管理（占用/清理策略） |

截图由 `.temp/smoke-html`（`cargo test dump_smoke_html -- --ignored` 导出）在
1920×1080 视口下渲染获得，与发行物同源同 CSS。

## 构建流程（Windows 机上）

```powershell
# 1) [macOS] scripts/build-windows.sh 出 NSIS → 拷到 Windows 机安装并测试
#    （功能验收在 NSIS 渠道完成；MSIX 只出商店提交包，无需本地测试证书）

# 2) [Windows] 装 WinApp CLI（一次性）：winget install Microsoft.winappcli --source winget

# 3) [每次] 打包（自动复制 manifest + 图标 + exe/dll 并调 winapp pack）：
powershell -ExecutionPolicy Bypass -File scripts\build-msix.ps1
#    产物：store-assets\msix\out\CorpMail_0.3.0.0_x64.msix（未签名，微软代签）
#    （可 -InstallDir 覆盖安装目录；-WinApp 指定 winapp.exe 路径）

# 4) Partner Center 提交（商店认证后微软代签）
```

## 版本规则

- MSIX `Version` 四段制、末段恒 `0`（商店强制）；构建脚本从 `Cargo.toml` 注入
- 每次商店提交版本号必须高于历史提交
