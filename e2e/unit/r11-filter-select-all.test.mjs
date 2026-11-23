// 第十一轮 P2-3：高级筛选（from/df/dt/att）下前端仍显示「跨页全选」入口，
// 点了却被后端 has_search 拒绝——前后端口径分叉。静态护栏钉死前端 isSearch
// 的判据集合必须与后端一致（q/from/df/dt/att/att=1）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');

const m = APP_JS.match(/function isSearch\(\) \{([\s\S]*?)\n    \}/);
assert.ok(m, 'app.js 必须含 isSearch 纯函数');
const body = m[1];

test('isSearch 覆盖后端 has_search 全部字段（P2-3 回归）', () => {
    for (const id of ['ba-q', 'ba-from', 'ba-df', 'ba-dt', 'ba-att']) {
        assert.ok(body.includes(`'${id}'`), `isSearch 必须判 ${id}：${body}`);
    }
    assert.match(body, /ba-att[^\n]*=== '1'/, 'att 须按值 1 判定（与后端 v==="1" 同口径）');
});
