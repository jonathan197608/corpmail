# 代码签名政策（Code Signing Policy）

本文档描述 CorpMail 的 Windows 发布物签名方式与团队职责，适用于本项目的发布/下载页面。

**Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)**

## 签名方式

- 发布构建在 GitHub Actions 上执行（公开仓库源码 → 自动化构建 → 提交签名请求）
- 每次发布需经过批准后签发
- 签名的二进制均可追溯到仓库源码与构建记录（SignPath origin verification）
- 安装时发布者显示为 **SignPath Foundation**

## 团队角色

本项目由个人维护（当前无组织团队）。

- **作者（Authors）**：可以直接修改源码的成员——当前为 [@jonathan197608](https://github.com/jonathan197608)（仓库管理员）；后续协作者以仓库 [Collaborators](https://github.com/jonathan197608/corpmail/settings/access) 为准
- **评审（Reviewers）**：非作者提交的变更（Pull Request）须由作者评审通过后合并
- **批准人（Approvers）**：每次发布签名须由下列成员批准：
  - `@jonathan197608`（仓库管理员）

## 签名产物

- Windows NSIS 安装器（`CorpMail_x.y.z_x64-setup.exe`）：主程序、WebView2Loader.dll、NSIS 插件与内嵌卸载器均被签名
- 核验：Windows 上 `signtool verify /pa` 或文件属性 → 数字签名

## 隐私

本程序不会向用户指定之外的网络系统传输任何信息，详见 [隐私声明](PRIVACY.md)。