// R17 P2：联系人建议 blur 后 150ms 内重新 focus，旧定时器仍到期隐藏
// 建议框 → 方向键/Enter 选人失效（mbxContactKey 首行见 hidden 直接 return）。
// 用源码护栏钉死 blur/focus 都先 clearTimeout(state.blurTimer)。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const app = readFileSync(
  path.join(path.dirname(fileURLToPath(import.meta.url)), '../../ui-dist/app.js'),
  'utf8'
);

test('R17 联系人建议 blur/focus 竞态：focus 取消在途隐藏定时器', () => {
  const at = app.indexOf("inp.addEventListener('blur'");
  assert.ok(at >= 0, 'blur 监听器存在');
  const end = app.indexOf("// --- P7 contact suggest core START ---", at);
  const frag = app.slice(at, end > at ? end : at + 600);
  assert.match(frag, /blurTimer/, '须用独立 blurTimer（不与输入防抖 timer 冲突）');
  assert.match(
    frag,
    /addEventListener\('focus'[\s\S]*?clearTimeout\(state\.blurTimer\)/,
    'focus 必须先清掉在途 blur 定时器（否则重新聚焦后建议框仍被隐藏）'
  );
  assert.match(
    frag,
    /addEventListener\('blur'[\s\S]*?clearTimeout\(state\.blurTimer\)/,
    '连续 blur 也须清掉前一个定时器'
  );
});
