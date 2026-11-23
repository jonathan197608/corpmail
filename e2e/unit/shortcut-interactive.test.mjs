// 第八轮 P2-4：列表页 Enter 快捷键不得劫持已聚焦按钮的原生激活。
// P3-2：Esc 收起操作菜单时须阻断传播，避免快捷键模块连带返回列表。
// 两个 handler 都挂 document，按注册顺序同步广播，用 stopImmediatePropagation  abort 链。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');

function slice(startMark, endMark) {
    const a = APP_JS.indexOf(startMark);
    const b = APP_JS.indexOf(endMark);
    assert.ok(a >= 0 && b > a, `app.js 必须带锚点 ${startMark}`);
    return APP_JS.slice(a, b);
}

const SHORTCUT_CORE = slice('// --- P8 shortcut core START ---', '// --- P8 shortcut core END ---');
const MENU_CORE = slice('// --- P8 split-menu esc core START ---', '// --- P8 split-menu esc core END ---');

function mkRow(linkHref) {
    return {
        classList: { toggle() {} },
        scrollIntoView() {},
        querySelector: (sel) => (sel === '.mr-link' && linkHref ? { href: linkHref } : null),
    };
}

// 快捷键模块 harness：加载 core，注入焦点/行/按钮，捕获 keydown handler
function shortcutHarness({ activeEl = null, withRows = true } = {}) {
    const calls = { defaultPrevented: 0, href: null };
    const handlers = [];
    const rows = withRows ? [mkRow('/emails/42'), mkRow('/emails/43')] : [];
    const doc = {
        body: { tagName: 'BODY' },
        documentElement: { tagName: 'HTML' },
        activeElement: activeEl,
        addEventListener: (type, fn) => { if (type === 'keydown') handlers.push(fn); },
        getElementById: (id) => (id === 'mail-list' ? { id } : null),
        querySelectorAll: (sel) => (sel === '#mail-list .mail-row' ? rows : []),
        querySelector: () => null,
    };
    const win = { location: { set href(v) { calls.href = v; } } };
    const ctx = vm.createContext({ document: doc, window: win, Array, console });
    vm.runInContext(SHORTCUT_CORE, ctx);
    const ev = {
        key: 'Enter',
        preventDefault() { calls.defaultPrevented++; },
        stopImmediatePropagation() {},
    };
    // 先 j 选中第一行，再 Enter
    const down = { key: 'j', preventDefault() {}, stopImmediatePropagation() {} };
    handlers.forEach((h) => h(down));
    handlers.forEach((h) => h(ev));
    return { calls };
}

// 菜单 + 快捷键两段广播 harness
function menuHarness({ menuOpen = true } = {}) {
    const calls = { back: 0, stopped: 0 };
    const handlers = [];
    const menus = menuOpen ? [{ hidden: false }] : [];
    const doc = {
        addEventListener: (type, fn) => { if (type === 'keydown') handlers.push(fn); },
        querySelectorAll: (sel) => (sel.includes('split-menu') ? menus : []),
        querySelector: (sel) => (sel === '.topbar .back' ? { click: () => { calls.back++; } } : null),
    };
    const ctx = vm.createContext({ document: doc, console });
    vm.runInContext(MENU_CORE, ctx); // 菜单 handler 先注册
    vm.runInContext(`
        document.addEventListener('keydown', function (ev) {
            if (ev.key === 'Escape') {
                var back = document.querySelector('.topbar .back');
                if (back) { ev.preventDefault(); back.click(); }
            }
        });
    `, ctx); // 快捷键模块后注册
    const ev = {
        key: 'Escape',
        stopImmediatePropagation() { calls.stopped++; this.__stopped = true; },
        preventDefault() {},
    };
    for (const h of handlers) {
        if (ev.__stopped) break;
        h(ev);
    }
    return { calls, menus };
}

test('焦点在按钮上时 Enter 放行原生激活，不劫持打开邮件（P2-4）', () => {
    const button = { tagName: 'BUTTON', closest: (s) => (s.includes('button') ? button : null) };
    const h = shortcutHarness({ activeEl: button });
    assert.equal(h.calls.defaultPrevented, 0, '不得 preventDefault 取消按钮激活');
    assert.equal(h.calls.href, null, '不得强制跳转打开邮件');
});

test('焦点在链接上时 Enter 同样放行（P2-4）', () => {
    const link = { tagName: 'A', closest: (s) => (s.includes('a[href]') ? link : null) };
    const h = shortcutHarness({ activeEl: link });
    assert.equal(h.calls.defaultPrevented, 0);
    assert.equal(h.calls.href, null);
});

test('焦点在邮件行/body 时 Enter 仍打开高亮邮件（回归）', () => {
    const h = shortcutHarness({ activeEl: { tagName: 'BODY', closest: () => null } });
    assert.equal(h.calls.href, '/emails/42', '焦点不在按钮时 Enter 须打开选中邮件');
    assert.equal(h.calls.defaultPrevented, 1);
});

test('Esc 收起展开菜单时阻断传播，不连带返回列表（P3-2）', () => {
    const h = menuHarness({ menuOpen: true });
    assert.equal(h.menus[0].hidden, true, '菜单须收起');
    assert.equal(h.calls.stopped, 1, '须阻断传播');
    assert.equal(h.calls.back, 0, '不得连带返回上一页');
});

test('无展开菜单时 Esc 放行，保留返回列表语义（P3-2 回归）', () => {
    const h = menuHarness({ menuOpen: false });
    assert.equal(h.calls.back, 1, '无菜单时 Esc 仍返回列表');
});
