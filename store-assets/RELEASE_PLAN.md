# CorpMail（Mailbox）· Microsoft Store 上架方案

> 版本：v1.1（2026-10-29）｜目标版本 0.3.0｜保留名 JonathanHuang.corpmail
> 本文档为执行总纲：阶段、步骤、责任分工、提交物与回退策略。
> v1.1 修订：修复 v1.0 编辑事故导致的全文档重复；统一安装路径为 CorpMail；
> 增加 WACK 通过后打 v0.3.0 tag 收尾步骤。

---

## 一、总体路线

```
[阶段 0] 资产定稿（本次会话完成）
    ↓
[阶段 1] 构建商店包（Windows 机，你执行）
    ↓
[阶段 2] Partner Center 提交（你执行，我提供逐字段清单）
    ↓
[阶段 3] 认证等待（1~7 天，通常 1~3 天）
    ↓
[阶段 4] 发布与双渠道并存管理
```

**渠道策略**：NSIS（GitHub Release 直发）与 Store（MSIX）长期并存。
NSIS 面向公司内网/Server 2016 等无商店环境；Store 面向公网用户。
AppData 虚拟化已禁用，两渠道数据互通；单实例互斥不跨渠道，文档已注明
"同机勿双渠道同时运行"。

## 二、阶段 1：构建商店包（Windows 机）

### 前置（一次性）
1. Windows 10/11 机器装 **Windows SDK**（任一近版，含 makeappx 与 WACK）
2. 该机已用 `CorpMail_0.3.0_x64-setup.exe` 安装应用（记下安装目录，默认
   `C:\Program Files\CorpMail`，下文以 `D:\CorpMail` 为例）

### 部署链路（已确认：macOS 编译，Windows 打商店包）

```
[macOS]   scripts/build-windows.sh
   └→ 产物 CorpMail_0.3.0_x64-setup.exe（NSIS）→ 拷贝到 Windows 机
[Windows] 运行安装器，安装 CorpMail（默认 C:\Program Files\CorpMail）
[Windows] 装 Windows SDK（含桌面工具）
[Windows] git clone 本仓库最新 main
   └→ powershell -ExecutionPolicy Bypass -File scripts\build-msix.ps1
        （自动探测安装目录与版本；可 -InstallDir "D:\CorpMail" 覆盖）
   └→ 产物 store-assets\msix\out\CorpMail_0.3.0.0_x64.msix（未签名，商店代签）
```

### 步骤
```powershell
# 1) 从 GitHub 拉取本仓库最新 main（含 store-assets/）
git clone https://github.com/jonathan197608/mailbox && cd mailbox

# 2) 打 MSIX（PowerShell 原生脚本：自动探测安装目录与版本，无需参数）
powershell -ExecutionPolicy Bypass -File scripts\build-msix.ps1
# 产物：store-assets/msix/out/CorpMail_0.3.0.0_x64.msix（未签名）

# 3) 本地安装验证（自签名用一下，仅本机测试用）
Add-AppxPackage -Path .\store-assets\msix\out\CorpMail_0.3.0.0_x64.msix -ForceApplicationShutdown
# 验证点：① 首启向导正常 ② 数据目录 = %APPDATA%\com.corpmail.desktop
#（若与 NSIS 版同装，确认无虚拟化重定向：安装后收几封信，再在 NSIS 版里能看到 = 数据互通）

# 4) WACK 认证预检
& "C:\Program Files (x86)\Windows Kits\10\App Certification Kit\appcert.exe" reset
& "...\appcert.exe" test -apptype desktop -appxpath "...\CorpMail_0.3.0.0_x64.msix"
# 要求：PASS，0 fail（FAIL 项按报告修后再提）
```

### 提交物
- `CorpMail_0.3.0.0_x64.msix`（上传商店）
- WACK 报告截图（留档）

### 收尾（WACK 通过后，Mac 侧）
- 打版本基线 tag（已确认此时机）：
  `git tag v0.3.0 && git push origin v0.3.0`
  （遵循 v0.1.0 / v0.2.3 惯例；tag 落在提交商店的最终 commit 上）

## 三、阶段 2：Partner Center 逐字段提交（按 store-listing.md）

> 全字段中英文本已备好在 `store-assets/listing/store-listing.md`，
> 此处仅列操作顺序与易错点。

| # | 页面 | 关键动作 | 易错点 |
|---|---|---|---|
| 1 | 定价和可用性 | 市场=全部默认；受众=公共；基价=免费 | 无 |
| 2 | 属性 | 类别=Productivity（子类别 Email 如有）；**隐私策略 URL 填** `https://github.com/jonathan197608/mailbox/blob/main/PRIVACY.zh-CN.md`；网站 `https://github.com/jonathan197608/mailbox`；支持联系方式=邮箱 | 隐私 URL 必须先推 GitHub 生效再提交 |
| 3 | 年龄分级 | 问卷全"否" → 3+ | 无 |
| 4 | 包 | 上传 `.msix`（未签名，商店代签） | 拒收时先查 Publisher 逐字符 |
| 5 | 商店一览 | 按 store-listing.md 粘贴 zh-CN/en-US 两份；5 张截图按清单上传 | 截图 ≥1366×768（我们 1920×1080） |
| 6 | 提交选项 | **认证备注粘贴 store-listing.md §2 英文 note** | 不写备注，审核员可能因"需账号"给"无法验证" |
| 7 | 提交认证 | — | 提交后状态"认证"即进入等待 |

## 四、阶段 3：认证期

- 新应用通常 **1~3 个工作日**（最长 7 天）
- 遭驳回的 TOP 原因与预案：
  - `身份不一致`：Publisher/Name 逐字比对 Partner Center 产品标识页
  - `WACK 失败`：本地 appcert 先跑（阶段 1 已做）
  - `隐私`：确认 URL 公网可访问
  - `WebView2 缺失启动失败`：商店测试机一般已装；备注已说明 + 兜底引导
- 收到审核意见 48h 内可回复解释或改包重提

## 五、阶段 4：发布

- 认证通过默认即发布（也可选手动发布）
- 发布后动作：
  1. README 加商店徽章与链接（`https://apps.microsoft.com/detail/9NXXXXXXXX`）
  2. store-assets/msix/README.md 记录 Store ID
  3. 后续版本走同链路：bump 版本 → NSIS → MSIX → 提交（版本号必须递增）

## 六、回退策略

| 场景 | 处置 |
|---|---|
| 商店包有致命缺陷 | Partner Center「停止获取」→ 修后提新版本（版本号+1） |
| 想下架 | 提交里选"停止提供"，已装用户不受影响 |
| MSIX 与 NSIS 数据冲突 | 虚拟化已禁用理论上不发生；若发生：停止商店渠道分发，回归 NSIS 单渠道 |

## 七、当前状态清单（Assets readiness）

| 项 | 状态 |
|---|---|
| AppxManifest（Identity/Publisher/语言 zh-CN/虚拟化禁用） | ✅ 定稿 |
| 商店图标 6 规格（44/50/150/310/620 + Wide） | ✅ |
| listing 中英双语 + 认证备注英文 | ✅ |
| 隐私声明（zh-CN 主用 + en-US 备用） | ✅ 本次推根目录公开路径 |
| 截图 5 张 1920×1080 | ✅ |
| build-msix.ps1（PowerShell 原生）+ WACK 校验提示 | ✅ |
| 版本 0.3.0 全链统一 | ✅ |
| identifier/数据目录 com.corpmail.desktop | ✅ 本版统一（无存量用户，无需迁移） |
| 本体品牌 CorpMail（productName/窗口标题/页面/启动动画） | ✅ 本版统一 |
| 公开隐私文档路径（仓库根） | ✅ 本次修复（原 docs/ 整体在 .gitignore，链接会 404） |
| GitHub 仓库名保持 mailbox | ✅ 已确认不改（商店隐私链接指向该路径） |
