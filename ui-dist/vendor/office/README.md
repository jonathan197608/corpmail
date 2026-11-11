# Office 预览前端库（内嵌）

本目录为预构建产物，项目本身无 Node 构建链；升级时整包替换并更新校验和。
所有文件均从 npm 官方包解包取得，运行时不访问外网。

| 文件 | 来源 | 版本 | 许可证 | SHA-256 |
|---|---|---|---|---|
| `docx-preview.min.js` | npm `docx-preview` | 0.4.1 | Apache-2.0 | `c4a133c65a112799e35b143c572dfff9e923a3bf39b5fd3b07a06aa0f8f530c2` |
| `jszip.min.js` | npm `jszip` | 3.10.1 | MIT（见 LICENSE-jszip.md） | `acc7e41455a80765b5fd9c7ee1b8078a6d160bbbca455aeae854de65c947d59e` |
| `xlsx.full.min.js` | npm `xlsx` | 0.18.5 | Apache-2.0（见 LICENSE-sheetjs.txt） | `c9506197caf809a075b6dee1da0d36fb19da7158ffe8a88e7b0c96c5d8623c99` |
| `pptx-preview.umd.js` | npm `pptx-preview` | 1.0.7 | package.json 标注 ISC；README 声明 npm 包可免费商用、源码不公开（见下方说明） | `98bcf8fbb5efb42dff406eeea7e7b9d7497a388955582e5b1815489c63c75fe2` |

## 各库许可与商用条件（逐条列出，不按「个人项目」概括）

| 库 | 许可 | 商用 | 依据 |
|---|---|---|---|
| docx-preview | Apache-2.0 | 允许，需保留版权与许可声明 | 包内 LICENSE 原文（Apache-2.0 全文） |
| jszip | MIT | 允许，需保留版权与许可声明 | `LICENSE-jszip.md` |
| xlsx（SheetJS 社区版） | Apache-2.0 | 允许；**注意**社区版仅覆盖读写表格，不含 SheetJS Pro 的商业特性 | `LICENSE-sheetjs.txt` |
| pptx-preview | package.json 标注 ISC | npm 发布产物可免费使用（含商用）；**源码不公开**，不得据其源码二次分发 | 上游 README「发布的 npm 包可免费使用，自用商用均可」 |

- 本项目只内嵌各库的 **npm 发布产物**，不修改、不反编译、不再分发源码。
- `pptx-preview` 的授权来自上游 README 的明确表述（非标准 SPDX 许可文件），因此单列在上表：若日后上游收紧条款，前端退化为「无法预览，请下载后用 PowerPoint 打开」，其余格式不受影响。
- Apache-2.0 库的 NOTICE/版权声明随 `LICENSE-*.txt` 一并保留在本目录，随应用分发。

## 加载方式

`office-preview.js` 在用户首次点击 Office 附件时按需插入 `<script>`，仅加载对应格式所需文件：

- DOCX：`jszip.min.js` + `docx-preview.min.js`
- XLSX/XLS：`xlsx.full.min.js`
- PPTX：`pptx-preview.umd.js`

升级步骤：替换文件 → 更新本表 SHA-256 → 运行 `cargo test` 与 Office 预览冒烟。