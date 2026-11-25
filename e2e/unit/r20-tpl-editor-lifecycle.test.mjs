// R20 P2：设置页模板 pane 切走/保存后整片 swap，旧 wangEditor 实例必须销毁，
// 否则反复切 tab 会累积泄漏（DOM 监听/定时器随实例挂在被摘除的节点上）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const a = APP_JS.indexOf('// --- R20 tpl lifecycle core START ---');
const b = APP_JS.indexOf('// --- R20 tpl lifecycle core END ---');
assert.ok(a >= 0 && b > a, 'app.js 必须带 R20 tpl lifecycle core 锚点');
const CORE = APP_JS.slice(a, b);

function harness() {
    const listeners = {};
    const ctx = vm.createContext({
        console,
        document: {
            body: {
                addEventListener(type, fn) {
                    (listeners[type] ||= []).push(fn);
                },
            },
        },
    });
    vm.runInContext(CORE, ctx);
    return {
        ctx,
        fireSwap(targetId) {
            for (const fn of listeners['htmx:beforeSwap'] || []) {
                fn({ detail: { target: { id: targetId } } });
            }
        },
    };
}

test('settings-pane 换入前销毁所有登记的编辑器实例', () => {
    const h = harness();
    let destroyed = 0;
    const ed = { destroy() { destroyed += 1; } };
    const field = { __tplSt: { ed } };
    h.ctx.MBX_TPL_REGISTRY.push(field);

    h.fireSwap('settings-pane');

    assert.equal(destroyed, 1, '实例必须被销毁一次');
    assert.equal(field.__tplSt.ed, null, '销毁后引用须置空，防二次 destroy');
    assert.equal(h.ctx.MBX_TPL_REGISTRY.length, 0, '注册表须清空，新 pane 重新登记');
});

test('销毁失败不阻断后续实例；非 settings-pane swap 不误清', () => {
    const h = harness();
    let second = 0;
    const bad = {
        __tplSt: {
            ed: {
                destroy() {
                    throw new Error('已销毁');
                },
            },
        },
    };
    const good = { __tplSt: { ed: { destroy() { second += 1; } } } };
    h.ctx.MBX_TPL_REGISTRY.push(bad, good);

    h.fireSwap('mail-list');
    assert.equal(second, 0, '其他区域 swap 不得误清编辑器');
    assert.equal(h.ctx.MBX_TPL_REGISTRY.length, 2);

    h.fireSwap('settings-pane');
    assert.equal(second, 1, '前一个销毁抛异常不得阻断后续实例');
    assert.equal(bad.__tplSt.ed, null, '抛异常的实例也要置空引用');
    assert.equal(h.ctx.MBX_TPL_REGISTRY.length, 0);
});

test('初始化路径确实登记字段（源码护栏，收窄到 mbxTplInitField）', () => {
    const at = APP_JS.indexOf('function mbxTplInitField(field)');
    assert.ok(at >= 0, '初始化函数存在');
    const end = APP_JS.indexOf('function mbxTplInitAll(', at);
    const body = APP_JS.slice(at, end);
    assert.ok(
        body.includes('MBX_TPL_REGISTRY.push(field)'),
        '每个初始化字段须登记到注册表，否则 beforeSwap 无从销毁'
    );
    assert.ok(
        body.includes('field.__tplSt = st'),
        '字段须挂 st 引用，销毁时才能拿到 ed'
    );
});
