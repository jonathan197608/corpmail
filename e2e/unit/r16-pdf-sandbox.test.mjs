// R16 P2：PDF 附件预览 iframe 必须与 Office 预览同口径加 sandbox + no-referrer。
// 同源 iframe 默认可触达 window.__TAURI__，crafted PDF 的 JS/URI 动作是
// 纵深防御缺口。用源码护栏钉死（前端无构建链，行为测试需真 WebView）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const app = readFileSync(
  path.join(path.dirname(fileURLToPath(import.meta.url)), '../../ui-dist/app.js'),
  'utf8'
);

test('R16 PDF 预览 iframe 带 sandbox 与 no-referrer', () => {
  const at = app.indexOf("kind === 'pdf'");
  assert.ok(at >= 0, 'pdf 分支存在');
  const end = app.indexOf("kind === 'text'", at);
  const branch = app.slice(at, end > at ? end : at + 600);
  assert.match(
    branch,
    /setAttribute\(\s*['"]sandbox['"]\s*,/,
    'PDF iframe 必须设置 sandbox（不得同源裸 iframe）'
  );
  assert.match(
    branch,
    /referrerpolicy/,
    'PDF iframe 必须设置 referrerpolicy=no-referrer'
  );
  const setAttr = branch.match(/setAttribute\(\s*['"]sandbox['"]\s*,\s*['"]([^'"]*)['"]\s*\)/);
  assert.ok(setAttr, 'sandbox 设置必须是字面量以便审计');
  assert.ok(
    !setAttr[1].includes('allow-scripts'),
    'sandbox 字面量不得含 allow-scripts（PDF 阅读器不需要脚本）'
  );
});
