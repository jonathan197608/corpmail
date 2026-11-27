// 第七轮 P2-5：设置页 radio 组（首次拉取范围 7/30/90/0）改动必须被脏判定识别。
// 旧 snapshot 对同名 radio 逐个覆盖，最后一个未选中项把值写成 '' →
// 7/30/90 互改快照恒等，保存按钮禁用、离开不提示，改动静默丢失。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(path.join(here, '../../ui-dist/app.js'), 'utf8');
const START = '// --- P7 settings snapshot core START ---';
const END = '// --- P7 settings snapshot core END ---';
const a = APP_JS.indexOf(START);
const b = APP_JS.indexOf(END);
assert.ok(a >= 0 && b > a, 'app.js 必须带 P7 settings snapshot core 锚点');
const CORE = APP_JS.slice(a, b);

function radioForm(checked) {
    return {
        querySelectorAll() {
            return ['7', '30', '90', '0'].map((v) => ({
                name: 'first_fetch_days',
                type: 'radio',
                value: v,
                checked: v === checked,
                disabled: false,
            }));
        },
    };
}

test('radio 组各选中值产生不同快照（基线旧实现恒为同一空值）', () => {
    const snap = vm.runInContext(CORE + '\nmbxSnapshotForm', vm.createContext({}));
    const s7 = snap(radioForm('7'));
    const s30 = snap(radioForm('30'));
    const s90 = snap(radioForm('90'));
    const s0 = snap(radioForm('0'));
    assert.ok(s7.includes('7'), `选中 7 的快照须含 "7"：${s7}`);
    assert.ok(s30.includes('30'), `选中 30 的快照须含 "30"：${s30}`);
    assert.ok(s90.includes('90'));
    assert.ok(s0.includes('0'));
    assert.equal(new Set([s7, s30, s90, s0]).size, 4, '四种选中态快照必须互不相同');
    assert.notEqual(s7, s30, '7→30 必须产生脏判定差异');
    assert.notEqual(s30, s90, '30→90 必须产生脏判定差异');
});

test('设置页脏判定 IIFE 确实使用 core 快照（不是镜像副本）', () => {
    // 旧护栏只查全文件 contains：把赋值挪进无关 IIFE、设置页改用镜像快照
    // 仍绿。收窄到 P7 core END 之后紧邻的 IIFE 头部。
    const coreEnd = APP_JS.indexOf('// --- P7 settings snapshot core END ---');
    assert.ok(coreEnd >= 0, 'P7 core 锚点存在');
    const iifeHead = APP_JS.slice(coreEnd, coreEnd + 240);
    assert.match(
        iifeHead,
        /\(function \(\) \{[\s\S]{0,160}var snapshot = mbxSnapshotForm;/,
        'snapshot 必须由紧随 P7 core 的设置页 IIFE 直接引用 core 函数'
    );
});
