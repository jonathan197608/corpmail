// R21 P1-2：会话页置顶翻转后卡片头星标必须同步。
// 服务端模板按 m.starred 渲染 .tm-star，而按钮走 hx-swap=none + 事件就地翻转，
// 不同步会出现「按钮已置顶、卡片无星」。这里把真实的 mbx:starDone 监听器放进
// Node vm，用假 DOM 驱动翻两种方向，并钉死「按 id 精确匹配按钮」。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = "document.body.addEventListener('mbx:starDone', function (ev) {";

function listenerSrc() {
    const start = APP_JS.indexOf(START);
    assert.ok(start >= 0, 'app.js 必须含 mbx:starDone 监听器');
    const end = APP_JS.indexOf('\n});', start);
    assert.ok(end > start, '监听器边界异常');
    return APP_JS.slice(start, end + 4);
}

function harness({ starredBadge = null } = {}) {
    let handler = null;
    const created = [];
    let removed = 0;
    const btn = {
        className: 'row-act star-btn btn small thread-star on',
        _classes: new Set(['on']),
        attrs: {},
        title: '',
        innerHTML: '',
        textContent: '已置顶',
        classList: {
            toggle(name, on) {
                if (on) btn._classes.add(name); else btn._classes.delete(name);
            },
        },
        setAttribute(k, v) { btn.attrs[k] = v; },
        closest(sel) { return sel === '.thread-mail-wrap' ? wrap : null; },
    };
    let badge = starredBadge;
    if (badge) badge.remove = () => { removed++; };
    const head = { appendChild() { /* createElement 已记账，避免重复计数 */ } };
    const wrap = { querySelector(sel) {
        if (sel === '.tm-head') return head;
        if (sel === '.tm-star') return badge;
        return null;
    } };
    const document = {
        body: { addEventListener(n, f) { if (n === 'mbx:starDone') handler = f; } },
        getElementById: () => null, // 会话页无 list-area → 走就地翻转分支
        querySelector(sel) {
            return sel === '.star-btn[data-mail-id="42"]' ? btn : null;
        },
        createElement(tag) {
            const el = { tag, className: '', innerHTML: '' };
            created.push(el);
            return el;
        },
    };
    const ctx = vm.createContext({
        document, MBX_IC: { star: '<svg>star</svg>', starFill: '<svg>filled</svg>' },
        mbxRefreshList: () => { throw new Error('会话页不应走列表刷新'); },
        console,
    });
    vm.runInContext(listenerSrc(), ctx);
    return {
        fire: (detail) => handler({ detail }),
        btn, created, wrap,
        removed: () => removed,
        badge: () => badge,
    };
}

test('P1-2：按 id 精确翻按钮，置顶时插入同卡片头星标', () => {
    const h = harness();
    h.fire({ starred: true, id: 42 });
    assert.equal(h.btn.attrs['aria-pressed'], 'true', '按钮 aria-pressed 翻转');
    assert.ok(h.btn.innerHTML.includes('filled'), '按钮文案用实心星');
    const inserted = h.created.find((c) => c.className === 'tm-star');
    assert.ok(inserted, '卡片头须插入 .tm-star 徽标');
    assert.ok(inserted.innerHTML.includes('filled'), '徽标用实心星图标');
});

test('P1-2：取消置顶时移除同卡片头星标', () => {
    const existing = { className: 'tm-star', innerHTML: '<svg>filled</svg>' };
    const h = harness({ starredBadge: existing });
    h.fire({ starred: false, id: 42 });
    assert.equal(h.btn.attrs['aria-pressed'], 'false', '按钮 aria-pressed 翻转');
    assert.ok(h.btn.innerHTML.includes('star</svg>'), '按钮文案用空心星');
    assert.equal(h.removed(), 1, '已有徽标必须被移除');
});

test('P1-2：事件 id 不匹配任何按钮时不误翻（多卡片精确匹配）', () => {
    const h = harness();
    h.fire({ starred: true, id: 99 }); // DOM 只提供 id=42 的按钮
    assert.equal(h.created.length, 0, '无匹配按钮不得插入徽标');
    assert.equal(h.btn.attrs['aria-pressed'], undefined, '无匹配按钮不得翻转态');
});
