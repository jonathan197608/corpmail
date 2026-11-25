// 复查 R2：跨类型预览代次——慢 text 附件 A 的 fetch 在途时切到 Office 附件 B，
// A 的迟到响应不得把纯文本塞进 B 的面板。
//
// 旧实现 mbxPreviewAttachment 只在 text 分支 ++mbxPreviewSeq，Office 分支只调
// mbxOfficeReset（递增 office 内部 seq，不动本侧令牌）→ A 的 .then 判
// my === mbxPreviewSeq 成立 → 「B 文件名 + A 文本」。把 app.js 的
// A5 preview dispatch core 放进 Node vm，假 fetch（A 慢 / B 立即）驱动真分支。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- A5 preview dispatch core START ---';
const END = '// --- A5 preview dispatch core END ---';
const CLOSE_START = '// --- P5 preview close core START ---';
const CLOSE_END = '// --- P5 preview close core END ---';

function between(startMark, endMark) {
    const start = APP_JS.indexOf(startMark);
    const end = APP_JS.indexOf(endMark);
    assert.ok(start >= 0 && end > start, `app.js 必须带 ${startMark} 锚点`);
    return APP_JS.slice(start, end);
}

function core() {
    // 复查五 P3-1：关闭函数与入口必须同段执行——mbxPreviewSeq 是 let，
    // 分两次 vm.runInContext 会是不同词法作用域，关闭就无法换代次。
    return between(START, END) + '\n' + between(CLOSE_START, CLOSE_END);
}

function harness({ fetchPlan } = {}) {
    const mk = (tag) => {
        const el = {
            tagName: tag, _kids: [], dataset: {}, _l: {},
            addEventListener(n, f) { (this._l[n] = this._l[n] || []).push(f); },
            removeEventListener() {},
            querySelector: () => mk('inner'), querySelectorAll: () => [],
            setAttribute() {}, removeAttribute() {}, focus() {},
            appendChild(c) { this._kids.push(c); return c; },
            classList: { add: () => {}, remove: () => {}, toggle: () => {} },
        };
        const def = (n, get, set) => Object.defineProperty(el, n, { get, set, configurable: true });
        def('className', function () { return this._c || ''; }, function (v) { this._c = v; });
        def('textContent',
            function () { return this._t || ''; },
            function (v) { this._t = v; this._kids = []; });
        def('innerHTML',
            function () { return this._h || ''; },
            function (v) { this._h = v; if (v === '') this._kids = []; });
        def('hidden', function () { return this._hidden || false; }, function (v) { this._hidden = v; });
        return el;
    };
    const body = mk('pm-body');
    const nameEl = mk('pm-name');
    const modal = mk('preview-modal');
    modal.querySelector = () => mk('panel');
    const byId = { 'pm-body': body, 'pm-name': nameEl, 'preview-modal': modal };
    const documentEl = {
        getElementById: (id) => byId[id] || null,
        createElement: (tag) => mk(tag),
    };
    const getTextNodes = () => body._kids.map((k) => k._t || '');

    const fetchImpl = async (url) => {
        const plan = fetchPlan && fetchPlan[url];
        if (!plan) throw new Error('no plan for ' + url);
        if (plan.delayMs > 0) await new Promise((r) => setTimeout(r, plan.delayMs));
        return {
            ok: true, status: 200,
            text: async () => plan.text || '',
        };
    };

    const officeCalls = [];
    const resetCalls = [];
    const windowObj = {
        mbxOfficeReset: () => resetCalls.push('reset'),
        mbxPreviewOffice: (url, kind, name) => {
            officeCalls.push(name);
            // Office 分支立即「渲染完成」：往 body 塞一个带来源标记的节点
            const el = mk('iframe');
            el._t = 'OFFICE-B-CONTENT';
            body.appendChild(el);
        },
    };
    const ctx = vm.createContext({
        document: documentEl,
        fetch: fetchImpl,
        window: windowObj,
        mbxPreviewTrap: () => {},
        console,
    });
    vm.runInContext(core(), ctx);
    return {
        ctx, body, nameEl, officeCalls, resetCalls, getTextNodes,
        preview: (url, kind, name) => ctx.mbxPreviewAttachment(url, kind, name),
        close: () => ctx.mbxClosePreview(),
    };
}

test('R2：慢 text A 在途切到 Office B，最终面板不含 A 文本（跨类型代次）', async () => {
    const h = harness({
        fetchPlan: {
            '/a/view': { delayMs: 200, text: 'A-TEXT-SHOULD-NOT-APPEAR' },
        },
    });
    h.preview('/a/view', 'text', 'A.txt'); // A：text fetch 在途
    await new Promise((r) => setTimeout(r, 10));
    h.preview('/b/view', 'docx', 'B.docx'); // B：Office 立即完成
    await new Promise((r) => setTimeout(r, 260)); // 等 A 的迟到响应

    assert.deepEqual(h.officeCalls, ['B.docx'], 'Office 分支应被调用一次');
    // R18：resetCalls 以前只记录不断言，删掉 app.js 的 mbxOfficeReset 仍能全绿。
    assert.equal(h.resetCalls.length, 2, 'A 与 B 两次打开都须复位 Office 代次');
    const allText = h.getTextNodes().join('\n');
    assert.ok(
        !allText.includes('A-TEXT-SHOULD-NOT-APPEAR'),
        `迟到的 text A 不得写入 B 面板（body：${allText}）`,
    );
    assert.ok(h.getTextNodes().some((t) => t.includes('OFFICE-B-CONTENT')), 'B 内容应在面板');
    assert.match(h.nameEl.textContent, /B\.docx/, '文件名应为 B');
});

test('R2：正常 text 单附件渲染不受影响（无回归）', async () => {
    const h = harness({
        fetchPlan: { '/only/view': { delayMs: 0, text: 'ONLY-TEXT' } },
    });
    h.preview('/only/view', 'text', '唯一.txt');
    await new Promise((r) => setTimeout(r, 30));
    assert.ok(h.getTextNodes().some((t) => t.includes('ONLY-TEXT')), 'text 分支应正常渲染');
    assert.equal(h.resetCalls.length, 1, 'text 分支也须走统一入口复位 Office 代次');
});

test('P3-1：慢 text 在途关闭预览，迟到响应不得写入隐藏面板', async () => {
    const h = harness({
        fetchPlan: { '/slow/view': { delayMs: 120, text: 'SHOULD-NOT-APPEAR' } },
    });
    h.preview('/slow/view', 'text', '慢.txt'); // text fetch 在途
    await new Promise((r) => setTimeout(r, 10));
    h.close(); // 关闭：应换代次
    await new Promise((r) => setTimeout(r, 160)); // 等迟到响应
    assert.ok(
        !h.getTextNodes().some((t) => t.includes('SHOULD-NOT-APPEAR')),
        `关闭后迟到的 text 不得塞回隐藏面板（body：${h.getTextNodes().join('\n')}）`,
    );
});
