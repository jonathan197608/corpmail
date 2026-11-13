// C14：发送前「空正文 / 空主题」确认状态机的行为测试。
//
// 旧实现在放行分支里 delete 掉 mbxBodyOk / mbxSubjOk，导致「空正文带附件 +
// 空主题」要弹 3 次确认、发 4 次点击才真正提交（R40 只修了标志残留，没修
// 重复确认）。Rust 侧的 APP_JS.contains 静态护栏看不出这种分支差异，这里把
// app.js 的 C14 core 原样放进 Node vm，用假 DOM 连续驱动三次点击验证。
//
// 运行：node --test e2e/unit/compose-confirm.test.mjs（e2e/run.sh 0/6 步自动跑）
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- C14 compose confirm core START ---';
const END = '// --- C14 compose confirm core END ---';

function harness(fields = { to_addrs: 'a@b.c', subject: '', body_text: '' }, attach = true) {
    const confirms = [];
    const registered = [];
    const notes = [];
    const card = {
        firstChild: null,
        querySelector: () => null,
        insertBefore: (node) => notes.push(node.textContent),
    };
    const elt = {
        dataset: {},
        matches: (s) => s === 'button[hx-post="/compose/send"]',
        closest: (s) => (s === 'form' ? form : null),
        click: () => {},
    };
    const form = {
        querySelector: (sel) => {
            if (sel === '#keep-ids') return { value: attach ? '[1]' : '[]' };
            if (sel === '#body-mode') return { value: 'text' };
            if (sel === '#body-text') return { value: fields.body_text || '' };
            const m = /^\[name="(.+)"\]$/.exec(sel);
            if (m) return { value: fields[m[1]] || '' };
            return null;
        },
        closest: () => card,
    };
    const ctx = vm.createContext({
        document: {
            body: { addEventListener: (name) => registered.push(name) },
            createElement: () => ({
                className: '', textContent: '', innerHTML: '',
                querySelector: () => null,
            }),
        },
        mbxEditor: null,
        pickedPaths: [],
        // 立即执行回调 = 模拟用户点了「仍要发送」；真实链路里由 setTimeout
        // 在 550ms 后重触发点击，这里手动驱动下一次 fire() 代替
        mbxConfirmLeave: (msg, onConfirm) => {
            confirms.push(msg);
            onConfirm();
        },
        setTimeout: () => 0,
    });
    const start = APP_JS.indexOf(START);
    const end = APP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'app.js 必须带 C14 core 锚点');
    vm.runInContext(APP_JS.slice(start, end), ctx);
    return {
        confirms, registered, notes, elt,
        fire: () => {
            let prevented = false;
            ctx.mbxComposePrecheck({
                detail: { elt },
                preventDefault: () => { prevented = true; },
            });
            return prevented;
        },
        finishRequest: () => ctx.mbxComposeConfirmReset({ detail: { elt } }),
    };
}

test('空正文 + 空主题只弹两次确认，第三次点击直接放行', () => {
    const h = harness();

    assert.equal(h.fire(), true, '第一次应拦下并弹正文确认');
    assert.equal(h.confirms.length, 1);
    assert.match(h.confirms[0], /正文为空/);
    assert.equal(h.elt.dataset.mbxBodyOk, '1');

    assert.equal(h.fire(), true, '第二次应拦下并弹主题确认');
    assert.equal(h.confirms.length, 2);
    assert.match(h.confirms[1], /主题为空/);
    assert.equal(h.elt.dataset.mbxSubjOk, '1');
    // 关键回归点：旧实现在这里删掉 bodyOk，第三次点击又弹正文确认
    assert.equal(h.elt.dataset.mbxBodyOk, '1', '放行分支不得清掉正文确认标志');

    assert.equal(h.fire(), false, '第三次必须放行，不得再弹确认');
    assert.equal(h.confirms.length, 2, '总确认次数必须是 2 而不是 3');
});

test('两个确认标志只由真实请求结束事件清理', () => {
    const h = harness();
    h.fire();
    h.fire();
    assert.equal(h.elt.dataset.mbxBodyOk, '1');
    assert.equal(h.elt.dataset.mbxSubjOk, '1');
    // 请求失败/成功都会触发 afterRequest：标志必须同时清空，
    // 否则下次点发送会静默跳过确认
    h.finishRequest();
    assert.equal(h.elt.dataset.mbxBodyOk, undefined);
    assert.equal(h.elt.dataset.mbxSubjOk, undefined);
});

test('空正文且无附件是硬拦，不进确认态', () => {
    const h = harness({ to_addrs: 'a@b.c', subject: '有主题', body_text: '' }, false);
    assert.equal(h.fire(), true);
    assert.equal(h.confirms.length, 0, '无附件空正文不该弹确认');
    assert.ok(h.notes.some((t) => t && t.includes('正文不能为空')), '应插入错误条');
});

test('两个处理器都注册到 body 上（接线不得漏）', () => {
    const h = harness();
    assert.deepEqual(
        [...h.registered].sort(),
        ['htmx:afterRequest', 'htmx:configRequest'],
    );
});
