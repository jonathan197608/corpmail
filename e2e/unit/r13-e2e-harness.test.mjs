// 第十三轮 E2E 护栏：fail-open 修复（mock 进程泄漏、恒真 wait、stale DOM）
// 用静态结构断言钉死——这些是 wdio 配置/spec 层，无法用 vm 桩执行。
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..');
const read = (p) => readFileSync(join(root, p), 'utf8');

test('onComplete 必须 await mock 清理，不能动态 import().then() 后同步返回', () => {
    const src = read('e2e/wdio.conf.mjs');
    const at = src.indexOf('onComplete:');
    assert.ok(at >= 0, 'onComplete 钩子存在');
    const body = src.slice(at, src.indexOf('\n  },', at));
    assert.match(body, /async function/, '钩子须 async，wdio 才会等 Promise');
    assert.match(body, /await import\('\.\/support\/mock\.mjs'\)/, '须 await 动态导入');
    assert.match(body, /stopMock\(\)/, '须显式调用 stopMock');
    assert.doesNotMatch(body, /\.then\(\(\{ stopMock \}\) => stopMock\(\)\)/, '旧 fire-and-forget 形态必须消失');
});

test('B9 侧栏未读角标断言不得含「0 封新」恒真兜底', () => {
    const src = read('e2e/specs/04-list.spec.mjs');
    const at = src.indexOf("await shot('侧栏-未读角标')");
    assert.ok(at >= 0, 'B9 用例存在');
    const before = src.slice(Math.max(0, at - 700), at);
    assert.doesNotMatch(before, /includes\('0 封新'\)/, '与角标无关的兜底会让 wait 恒真');
    assert.match(before, /\.side \.unread-chip/, '须等角标本身');
    assert.match(before, /toBeGreaterThan\(0\)/, '须断言数值 > 0，不能只等元素存在');
});

test('T2 六 pane 几何循环须有 stale-DOM 守卫（旧 pane 首帧）', () => {
    const src = read('e2e/specs/03-settings.spec.mjs');
    const loop = src.indexOf("for (const tab of ['send', 'receive', 'sync', 'storage', 'templates', 'about'])");
    assert.ok(loop >= 0, 'T2 六 tab 循环存在');
    const body = src.slice(loop, src.indexOf('widths.push', loop));
    assert.match(body, /\.settings-tabs \.stab\.on/, '须等目标 tab 高亮，证明新 DOM 已挂载');
    assert.match(body, /tabLabels\[tab\]/, '须按 tab 断对应文案，不能只断元素存在');
});
