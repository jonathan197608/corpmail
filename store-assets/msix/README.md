# NanoMail · Microsoft Store 上架资产包

本目录是商店上架的**配置、文案与截图**资产（MSIX 包体由 `scripts/build-msix.sh` 在 Windows 上构建）。

## 目录结构

```
store-assets/
├── msix/
│   ├── AppxManifest.xml     # 商店包清单（Identity.Name=nanomail 已定稿；Publisher 为占位）
│   ├── assets/              # 全套商店图标（像素风，最近邻放大无损）
│   └── （out/ 构建产物，不入库）
├── listing/
│   ├── store-listing.md     # Partner Center 逐字段对照表（中英双语）
│   ├── PRIVACY.zh-CN.md     # 隐私声明（提交前发布到仓库 docs/ 并公开链接）
│   └── PRIVACY.en-US.md
└── screenshots/             # 1920×1080 商店截图 ×5（真实页面渲染）
```

## 上架前唯一阻塞项

1. **Publisher**：Partner Center → 应用「产品管理 → 查看应用标识详细信息」复制 Publisher 串，
   替换 `AppxManifest.xml` 中的 `CN=PLACEHOLDER-Publisher`。
2. **隐私政策 URL**：将 `listing/PRIVACY.zh-CN.md` 复制到仓库 `docs/PRIVACY.zh-CN.md` 并推送，
   listing 提交时填 GitHub 链接。

## 渠道共存说明（重要，写进提交备注已备）

- **NSIS 渠道**（官网/GitHub 直发）与 **Store 渠道**（MSIX）可同时装在一台机器上——
  两者身份标识不同，互不覆盖。
- AppData 虚拟化已声明禁用，两渠道读写**同一个** `%APPDATA%\com.mailbox.desktop` 数据目录：
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
# 1) 先出 NSIS 安装器（macOS 交叉编译）并安装到本机
# 2) MSIX 重打包（推荐改装目录 D:\Mailbox）
scripts/build-msix.sh
# 3) WACK 校验 → Partner Center 提交（.msix 上传，商店代签）
```

## 版本规则

- MSIX `Version` 四段制、末段恒 `0`（商店强制）；构建脚本从 `Cargo.toml` 注入
- 每次商店提交版本号必须高于历史提交
