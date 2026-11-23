// 第十一轮 P1-2：autosave 后端写失败回 HTTP 200 + {"ok":false}，前端不得
// 只看 status 就显示「已自动保存」。本测试直接执行 app.js 的 A10 core
// （含 mbxAutosaveRespOk），用假 htmx 返回 200/ok:false 验证判定。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- A10 migration draft flush core START ---';
const END = '// --- A10 migration draft flush core END ---';
const CORE = APP_JS.slice(APP_JS.indexOf(START), APP_JS.indexOf(END));

function ctx() {
    return vm.createContext({ Promise, JSON, setTimeout, clearTimeout, console });
}

test('mbxAutosaveRespOk：200 + ok:false 判失败（P1-2 回归）', () => {
    const c = ctx();
    vm.runInContext(CORE, c);
    const f = vm.runInContext('mbxAutosaveRespOk', c);
    assert.equal(f({ status: 200, responseText: '{"ok":false}' }), false,
        '后端写失败以 body.ok 为准，不能判成功');
    assert.equal(f({ status: 200, responseText: '{"ok":true,"draft_id":0}' }), true);
    assert.equal(f({ status: 409, responseText: '{"ok":false}' }), false);
    assert.equal(f({ status: 200, responseText: 'not-json' }), true,
        '非 JSON 通道保持 status 判定');
    assert.equal(f(null), false);
});

test('mbxAutosaveRequest 用 body.ok 判成功，200/ok:false 返回 ok:false（P1-2）', async () => {
    const calls = { ajax: [] };
    const form = {
        isConnected: true,
        values: { to_addrs: 'a@b.test', body_text: 'x' },
        querySelector: () => null,
    };
    const doc = {
        body: { addEventListener() {}, removeEventListener() {} },
    };
    const c = vm.createContext({
        Promise, JSON, FormData: class { constructor(f) { this.e = Object.entries(f.values); } forEach(cb) { this.e.forEach(([k, v]) => cb(v, k)); } },
        setTimeout: () => 1, clearTimeout: () => {},
        document: doc,
        pickedPaths: [], renderPicked: () => {},
        htmx: {
            ajax: (m, url, opts) => {
                calls.ajax.push({ url, opts });
                Promise.resolve().then(() => opts.handler(null, {
                    xhr: { status: 200, responseText: '{"ok":false}' },
                }));
                return Promise.resolve();
            },
        },
    });
    vm.runInContext(CORE, c);
    const r = await vm.runInContext('mbxAutosaveRequest', c)(form, doc, c.htmx);
    assert.equal(r.ok, false, '200+ok:false 必须向「已自动保存」提示返回失败');
});
