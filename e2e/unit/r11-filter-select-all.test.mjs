// R18：旧护栏只 contains 字段 id，把 `!== ''` 改成 `=== ''` 仍全绿——
// 高级筛选下前端继续显示「跨页全选」，后端却按 has_search 拒绝。
// 现在直接执行抽出的纯函数，覆盖每个字段的真/假两态。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const APP_JS = readFileSync(
    path.join(path.dirname(fileURLToPath(import.meta.url)), '../../ui-dist/app.js'),
    'utf8'
);
const START = '// --- R18 isSearch core START ---';
const END = '// --- R18 isSearch core END ---';
const a = APP_JS.indexOf(START);
const b = APP_JS.indexOf(END);
assert.ok(a >= 0 && b > a, 'app.js 必须带 R18 isSearch core 锚点');
const ctx = vm.createContext({});
vm.runInContext(APP_JS.slice(a, b), ctx);
const isSearch = (values) => ctx.mbxBatchIsSearch((id) => values[id] || '');

test('isSearch：全空与纯空白不算检索态', () => {
    assert.equal(isSearch({}), false);
    assert.equal(isSearch({ 'ba-q': '   ', 'ba-from': '\t' }), false);
    assert.equal(isSearch({ 'ba-att': '0' }), false);
});

test('isSearch：q/from/df/dt 任一非空即检索态（后端 has_search 同口径）', () => {
    for (const id of ['ba-q', 'ba-from', 'ba-df', 'ba-dt']) {
        assert.equal(isSearch({ [id]: 'x' }), true, `${id} 非空必须为检索态`);
    }
});

test('isSearch：att 只认字符串 1', () => {
    assert.equal(isSearch({ 'ba-att': '1' }), true);
    assert.equal(isSearch({ 'ba-att': 'true' }), false);
    assert.equal(isSearch({ 'ba-att': 1 }), false, 'DOM value 恒字符串，数字不得误判');
});
