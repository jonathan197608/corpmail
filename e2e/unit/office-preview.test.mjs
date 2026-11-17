// A5（audit 2026-11-16）：Office 附件预览代次令牌的行为测试。
//
// 旧实现 mbxOfficeReset 只清 DOM，不取消在途 async——点大 DOCX 附件 A（解析慢）
// 再点附件 B，A 的 showIframe 迟到完成后清空 #pm-body 塞回 A 的 iframe，而
// #pm-name 已是 B：用户看到「B 的文件名 + A 的内容」。把 office-preview.js 的
// A5 core 放进 Node vm，假 fetch（A 慢 500ms / B 立即）驱动真分支。
//
// 运行：node --test e2e/unit/office-preview.test.mjs（e2e/run.sh 0/6 步自动跑）
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const OP_JS = readFileSync(path.join(here, '../../ui-dist/office-preview.js'), 'utf8');
const START = '// --- A5 office preview core START ---';
const END = '// --- A5 office preview core END ---';

function core() {
    const start = OP_JS.indexOf(START);
    const end = OP_JS.indexOf(END);
    assert.ok(start >= 0 && end > start, 'office-preview.js 必须带 A5 core 锚点');
    return OP_JS.slice(start, end);
}

/// 假 DOM：覆盖渲染链用到的 API。#pm-body 记录子节点与其来源代（断言最终内容唯一）
function harness({ fetchPlan, noAbort = false } = {}) {
    let seqTag = 0;
    // 属性全部 Object.defineProperty + configurable:true：script 桩需要重配 src
    // （对象字面量 getter/setter 默认不可重配，defineProperty 抛 TypeError，
    // 异常落进 open 的 catch → showError——正是渲染链静默失败的根因）
    const mk = (tag) => {
        const el = {
            tagName: tag, id: tag, _kids: [], dataset: {}, _l: {},
            addEventListener(n, f) { (this._l[n] = this._l[n] || []).push(f); },
            removeEventListener() {},
            querySelector: (sel) => (/^\.pm-panel$/.test(sel) ? mk('panel') : mk('inner')),
            querySelectorAll: () => [],
            setAttribute() {}, getAttribute: () => null, hasAttribute: () => false,
            removeAttribute() {},
            appendChild(c) { c.__origin = c.__origin || ++seqTag; this._kids.push(c); return c; },
            remove() {},
            focus: () => {},
            classList: { add: () => {}, remove: () => {}, toggle: () => {} },
            offsetWidth: 10, offsetHeight: 10,
        };
        const def = (name, get, set) =>
            Object.defineProperty(el, name, { get, set, configurable: true });
        def('className', function () { return this._c || ''; }, function (v) { this._c = v; });
        def('textContent',
            function () { return this._t || ''; },
            function (v) { this._t = v; this._kids = []; });
        def('innerHTML',
            function () { return this._h || ''; },
            function (v) { this._h = v; if (v === '') this._kids = []; });
        def('title', function () { return this._title || ''; }, function (v) { this._title = v; });
        def('src', function () { return this._src || ''; }, function (v) { this._src = v; });
        def('type', function () { return 'type_1'; }, function (v) { this._type = v; });
        def('onload', function () { return this._onload; }, function (f) { this._onload = f; });
        def('onerror', function () { return this._onerror; }, function (f) { this._onerror = f; });
        return el;
    };

    const body = mk('pm-body'); body._kids = [];
    const nameEl = mk('pm-name');
    const modalEl = mk('preview-modal');
    const panelEl = mk('panel');
    modalEl.querySelector = (sel) => (sel === '.pm-panel' ? panelEl : null);
    const byId = {
        'pm-body': body,
        'pm-name': nameEl,
        'preview-modal': modalEl,
    };
    const head = mk('head');
    const documentEl = {
        getElementById: (id) => byId[id] || null,
        createElement: (tag) => mk(tag),
        createTextNode: (t) => ({ textContent: t }),
        head,
        body: mk('body'),
        querySelectorAll: () => [],
    };

    // URL.createObjectURL / revokeObjectURL 追踪（孤儿 blob 检测 + 渲染产物
    // 的归属鉴别：blob 内容进入 url 映射，断言最终 iframe 的产物属于谁）
    const blobUrls = new Set();
    const blobContent = new Map();
    const revoked = new Set();
    const URLStub = {
        createObjectURL: (blob) => {
            const u = 'blob:fake-' + (++seqTag);
            blobUrls.add(u);
            const parts = (blob && blob._parts) || [];
            blobContent.set(u, parts.map((x) => String(x)).join('\n'));
            return u;
        },
        revokeObjectURL: (u) => { revoked.add(u); blobUrls.delete(u); },
    };

    // fetch 计划：url → { delayMs, tag }（tag 编码进 bytes，renderAsync 解码
    // 出归属——区分「A 名 B 内容」错配的可辨别产物）
    const fetchLog = [];
    const encoder = new TextEncoder();
    const fetchImpl = async (url) => {
        const plan = fetchPlan && fetchPlan[url];
        fetchLog.push(url);
        if (!plan) throw new Error('no plan for ' + url);
        if (plan.delayMs > 0) await new Promise((r) => setTimeout(r, plan.delayMs));
        if (plan.error) throw new Error(plan.error);
        const bytes = encoder.encode(plan.tag || 'untagged');
        return {
            ok: true, status: 200,
            arrayBuffer: async () => bytes.buffer,
            text: async () => plan.text || '',
        };
    };

    // AbortController 用 Node 原生（globalThis 有）；无则补桩
    // window 预置（core 的 IIFE 顶层即向 window 挂 mbxOfficeReset 等）
    const windowObj = {};
    const ctx = vm.createContext({
        document: documentEl,
        URL: URLStub,
        fetch: fetchImpl,
        setTimeout, clearTimeout,
        // noAbort：abort() 空操作（fetch 不被中断）——构造「迟到响应已发出，
        // 只能靠 fetch 之后的代次校验兜底」的场景（V4 的鉴别力来源）
        AbortController: noAbort
            ? class { constructor() { this.signal = {}; } abort() {} }
            : AbortController,
        // vm context 不含宿主全局：showIframe 的 new Blob(...) 需要显式提供
        Blob: class { constructor(parts, opts) { this._parts = parts; this._type = opts && opts.type; } },
        window: windowObj,
        console: { error: (m) => console.log('[vm.err]', m), log: (m) => console.log('[vm.log]', m) },
    });
    // script 注入桩（DOCX 依赖 window.docx/renderAsync）。
    // renderAsync 从 bytes 解码 tag 写进产物——迟到附件的产物自带归属标记
    const decoder = new TextDecoder();
    class FakeRenderer {
        async renderAsync(buf, bodyHost, styleHost) {
            const tag = decoder.decode(new Uint8Array(buf));
            bodyHost._h = '<div class="docx-doc" data-from="' + tag + '">' + tag + '</div>';
            styleHost.textContent = 'p{}';
        }
    }
    const scriptHandlers = {};
    documentEl.createElement = (tag) => {
        // template：content.innerHTML 直通（sanitize/inlineBlobImages 需要完整
        // DOM 解析器，vm 里不跑真净化——目标只有代次行为）。mk 的 innerHTML
        // 是字面量 setter（不可重配），新属性直接挂在实例上覆盖原型读取
        if (tag === 'template') {
            const t = mk('template');
            // sanitizeElement/inlineBlobImages 只对 content.querySelectorAll 的
            // 空集合做无害遍历（vm 里不解析真实子树）
            t.content = { innerHTML: '', querySelectorAll: () => [] };
            Object.defineProperty(t, 'innerHTML', {
                set(v) {
                    this.__h = String(v);
                    this.content.innerHTML = this.__h;
                },
                get() { return this.__h || ''; },
                configurable: true,
            });
            return t;
        }
        const el = mk(tag);
        if (tag === 'script') {
            // loadScript 时序：先赋 src 再赋 onload 再 appendChild——真浏览器在
            // append 后触发 onload。桩在 src 赋值后 0ms 调 onload（此时已赋值）。
            // 不能对 mk 的 src 属性二次 defineProperty（字面量 setter 不可重配会
            // 抛 TypeError，恰好是此前 renderDocx 静默失败的原因）
            Object.defineProperty(el, 'src', {
                set(v) {
                    this.__src = v;
                    const self = this;
                    setTimeout(() => {
                        if (/docx-preview/.test(v)) windowObj.docx = { renderAsync: new FakeRenderer().renderAsync };
                        if (/jszip/.test(v)) windowObj.JSZip = {};
                        const fn = self.onload;
                        if (fn) fn();
                    }, 0);
                },
                get() { return this.__src || ''; },
                configurable: true,
            });
        }
        return el;
    };

    vm.runInContext(core(), ctx);
    ctx.__currentName = '';

    // 便捷入口：记录当前打开的名字（渲染产物带 data-from 标识来源）
    const open = (url, kind, name) => {
        ctx.__currentName = name;
        return windowObj.mbxPreviewOffice(url, kind, name);
    };
    return {
        ctx, open, body, nameEl, fetchLog, blobUrls, blobContent, revoked,
        seq: () => windowObj.mbxOfficeSeq(),
    };
}

/// 递归找按钮 / 收集文本（R3 纯文本兜底用例用）
function findButton(node, label) {
    if (!node || typeof node !== 'object') return null;
    if (node.textContent === label && node._l && node._l.click) return node;
    for (const kid of node._kids || []) {
        const found = findButton(kid, label);
        if (found) return found;
    }
    return null;
}
function collectText(node, out = []) {
    if (!node || typeof node !== 'object') return out;
    if (node._t) out.push(node._t);
    for (const kid of node._kids || []) collectText(kid, out);
    return out;
}

test('A5：慢附件 A 完成前点 B，最终面板只有 B 的内容', async () => {
    const h = harness({
        fetchPlan: {
            '/emails/1/attachments/a/office': { delayMs: 120, tag: 'ATTACH-A' },
            '/emails/1/attachments/b/office': { delayMs: 0, tag: 'ATTACH-B' },
        },
    });
    // A：慢（120ms 后才完成 fetch）
    const pA = h.open('/emails/1/attachments/a/view', 'docx', 'A-大文档.docx');
    await new Promise((r) => setTimeout(r, 10)); // A 在途中
    // B：立即完成并渲染（nameEl 由 open 前置改写——iframe.title 即来源于它）
    await h.open('/emails/1/attachments/b/view', 'docx', 'B-小文档.docx');
    // 等 A 的迟到响应（若未拦）到达 + script 桩异步链排空
    await Promise.allSettled([pA]);
    await new Promise((r) => setTimeout(r, 150));

    // 观测点：body 的 iframe 子节点（showIframe 写入）。A 迟到未拦时会清 body
    // 再塞 A 的 iframe——title 是写入时 #pm-name 的当前值
    const frames = h.body._kids.filter((n) => (n.tagName || '') === 'iframe');
    assert.equal(frames.length, 1, `最终应恰一个 iframe（实际 ${frames.length}）`);
    const content = h.blobContent.get(String(frames[0].src || frames[0]._src)) || '';
    assert.ok(content.includes('ATTACH-B'), `最终产物应属于 B（blob 内容：${content.slice(0, 120)}）`);
    assert.ok(!content.includes('ATTACH-A'), `A 的迟到渲染不得覆盖 B（blob 内容含 A）`);
});

test('A5：正常单附件渲染不受代次影响（无回归）', async () => {
    const h = harness({
        fetchPlan: { '/emails/1/attachments/only/office': { delayMs: 0, tag: 'ONLY' } },
    });
    await h.open('/emails/1/attachments/only/view', 'docx', '唯一.docx');
    await new Promise((r) => setTimeout(r, 120));
    assert.match(h.nameEl.textContent, /唯一/, '标题正常');
    const frames = h.body._kids.filter((n) => (n.tagName || '') === 'iframe');
    assert.equal(
        frames.length, 1,
        `正常路径应渲染出一个 iframe（实际 kids：${h.body._kids.map((k) => (k.tagName || '?') + '.' + (k.className || '')).join(',')}）`,
    );
    assert.match(String(frames[0].title), /唯一/, 'iframe title 应为当前附件名');
    assert.match(String(frames[0]._src || frames[0].src || ''), /^blob:fake-/, 'iframe 应以 blob URL 承载');
});

test('A5：reset() 递增代次并 abort 在途 fetch（旧代错误不落屏）', async () => {
    const h = harness({
        fetchPlan: {
            '/emails/1/attachments/slow/office': { delayMs: 150, error: null, tag: 'SLOW' },
        },
    });
    const p = h.open('/emails/1/attachments/slow/view', 'docx', '慢.docx');
    await new Promise((r) => setTimeout(r, 10));
    const seqBefore = h.seq();
    // 用户切换附件 → 入口 mbxPreviewAttachment 先调 mbxOfficeReset
    h.ctx.window.mbxOfficeReset();
    assert.ok(h.seq() > seqBefore, 'reset 必须递增代次');
    await Promise.allSettled([p]);
    await new Promise((r) => setTimeout(r, 30));
    // 旧代就算完成也不得写入（reset 已清 body，且 mbxPreview 未再被调用）
    assert.ok(
        !h.body.innerHTML.includes('慢.docx'),
        `旧代完成渲染不得写入面板（实际：${h.body.innerHTML.slice(0, 120)}）`,
    );
});

test('A5：blob URL 生命周期——每次渲染 revoke 旧的，无孤儿堆积', async () => {
    const h = harness({
        fetchPlan: {
            '/emails/1/attachments/x/office': { delayMs: 0, tag: 'X' },
            '/emails/1/attachments/y/office': { delayMs: 0, tag: 'Y' },
        },
    });
    await h.open('/emails/1/attachments/x/view', 'docx', 'X.docx');
    await new Promise((r) => setTimeout(r, 120));
    await h.open('/emails/1/attachments/y/view', 'docx', 'Y.docx');
    await new Promise((r) => setTimeout(r, 120));
    // 连续两次成功 open 不应堆积未 revoke 的 blob（第二条生成时第一条应被 revoke）
    assert.ok(h.blobUrls.size <= 2, `活跃 blob 数应受控（实际 ${h.blobUrls.size}）`);
    assert.ok(h.revoked.size >= 1, `旧 blob 应被 revoke（不泄漏，实际 revoked=${h.revoked.size}）`);
});

test('R3：纯文本兜底 fetch 在途时切附件，迟到文本不得覆盖新面板', async () => {
    // A 的 office 主链失败（plan.error）→ showError 带「纯文本预览」按钮。
    // 点按钮触发 showTextFallback（office-text 延迟 120ms）+ 期间 open B。
    // 旧实现 showTextFallback 无 my 校验：A 的迟到文本 b.innerHTML='' 覆盖 B。
    const h = harness({
        fetchPlan: {
            '/emails/1/attachments/a/office': { delayMs: 0, error: '解析失败A' },
            '/emails/1/attachments/a/office-text': { delayMs: 120, text: 'A-FALLBACK-TEXT' },
            '/emails/1/attachments/b/office': { delayMs: 0, tag: 'ATTACH-B' },
        },
    });
    // A 主链失败 → 错误态出现「纯文本预览」按钮
    await h.open('/emails/1/attachments/a/view', 'docx', 'A-失败.docx');
    await new Promise((r) => setTimeout(r, 10));
    const btn = findButton(h.body, '纯文本预览');
    assert.ok(btn, '失败态应提供「纯文本预览」按钮');
    // 点按钮：触发 showTextFallback（A 的文本提取延迟 120ms 在途）
    const click = btn._l.click[0];
    click();
    await new Promise((r) => setTimeout(r, 5));
    // 用户切到 B（立即完成渲染）
    await h.open('/emails/1/attachments/b/view', 'docx', 'B-快.docx');
    await new Promise((r) => setTimeout(r, 10));
    // B 先渲染完成；等 A 的纯文本迟到响应（若未拦，会覆盖 B）
    await new Promise((r) => setTimeout(r, 160));
    const text = collectText(h.body).join('\n');
    assert.ok(
        !text.includes('A-FALLBACK-TEXT'),
        `迟到的纯文本兜底响应不得覆盖 B（body 文本：${text.slice(0, 200)}）`,
    );
});

test('R3（正向）：不切附件时纯文本兜底必须真的渲染出文本（防半修复恒 return）', async () => {
    // H1：既有 R3 迟到用例只断「A 文本不出现」，半修复（按钮回调漏传 my →
    // my=undefined，showTextFallback 两个 await 后恒 return）也能通过。
    // 本用例断正向路径：A 主链失败 → 点「纯文本预览」→ 不切附件 → 文本必须出现。
    const h = harness({
        fetchPlan: {
            '/emails/1/attachments/a/office': { delayMs: 0, error: '解析失败A' },
            '/emails/1/attachments/a/office-text': { delayMs: 20, text: 'FALLBACK-SHOULD-APPEAR' },
        },
    });
    await h.open('/emails/1/attachments/a/view', 'docx', 'A.docx');
    await new Promise((r) => setTimeout(r, 10));
    const btn = findButton(h.body, '纯文本预览');
    assert.ok(btn, '失败态应提供「纯文本预览」按钮');
    const click = btn._l.click[0];
    click();
    // 等兜底 fetch（20ms）+ 两次 await 完成
    await new Promise((r) => setTimeout(r, 80));
    const text = collectText(h.body).join('\n');
    assert.ok(
        text.includes('FALLBACK-SHOULD-APPEAR'),
        `不切附件时纯文本兜底必须渲染出文本（body：${text.slice(0, 200)}）`,
    );
});

test('A5：abort 未生效时，fetch 之后的代次校验必须拦下迟到渲染（V4 兜底）', async () => {
    const h = harness({
        noAbort: true,
        fetchPlan: {
            '/emails/1/attachments/slowa/office': { delayMs: 120, tag: 'SLOW-A' },
            '/emails/1/attachments/fastb/office': { delayMs: 0, tag: 'FAST-B' },
        },
    });
    const pA = h.open('/emails/1/attachments/slowa/view', 'docx', 'A-慢附件.docx');
    await new Promise((r) => setTimeout(r, 10));
    await h.open('/emails/1/attachments/fastb/view', 'docx', 'B-快附件.docx');
    // A 的 fetch 迟到完成（未被 abort 拦下）
    await Promise.allSettled([pA]);
    await new Promise((r) => setTimeout(r, 150));

    const frames = h.body._kids.filter((n) => (n.tagName || '') === 'iframe');
    assert.equal(frames.length, 1, '最终应恰一个 iframe');
    const content = h.blobContent.get(String(frames[0].src || frames[0]._src)) || '';
    assert.ok(
        content.includes('FAST-B') && !content.includes('SLOW-A'),
        `迟到 A 必须被 fetch 后校验拦下（blob 内容：${content.slice(0, 120)}）`,
    );
});
