// CorpMail 前端辅助（M2：附件选择/另存，__TAURI__.dialog；设计 §2/§5.1）
// 全局 500ms 防抖由 htmx 触发器表达式承担（hx-trigger="... once:500ms"，设计 §7）

// 启动引导动画移除（M5：base.html 内联 #boot，脚本就绪即撤）
(function removeBoot() {
    const boot = document.getElementById('boot');
    if (boot) boot.remove();
})();

function escapeHtml(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;')
            .replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// ---------- 撰写页：待保存附件（dialog 选路径，存草稿时随表单提交） ----------

let pickedPaths = [];

async function mbxPickAttachments() {
    try {
        const files = await window.__TAURI__.dialog.open({ multiple: true });
        if (!files) return;
        for (const f of (Array.isArray(files) ? files : [files])) {
            if (!pickedPaths.includes(f)) pickedPaths.push(f);
        }
        renderPicked();
    } catch (e) {
        console.error('[mbx] 打开文件对话框失败', e);
    }
}

function mbxRemovePicked(i) {
    pickedPaths.splice(i, 1);
    renderPicked();
}

function renderPicked() {
    const input = document.getElementById('attach-paths');
    if (input) input.value = JSON.stringify(pickedPaths);
    const ul = document.getElementById('picked-list');
    if (!ul) return;
    ul.innerHTML = pickedPaths.map((p, i) => {
        const name = p.split(/[\\/]/).pop();
        return `<li class="att-row picked"><span class="att-clip">📎</span>` +
               `<span class="att-name">${escapeHtml(name)}</span>` +
               `<button type="button" class="btn small" onclick="mbxRemovePicked(${i})">移除</button></li>`;
    }).join('');
}

// 已入库附件的移除：从 keep_ids 剔除（存草稿时服务端裁剪）
function mbxRemoveKept(aid, btn) {
    const input = document.getElementById('keep-ids');
    try {
        const ids = JSON.parse(input.value || '[]');
        const idx = ids.indexOf(aid);
        if (idx >= 0) ids.splice(idx, 1);
        input.value = JSON.stringify(ids);
    } catch (e) {
        input.value = '[]';
    }
    btn.closest('li').remove();
}

// ---------- 详情页：附件另存为（dialog 选保存路径 → htmx POST） ----------
// 请求护栏（M5 防抖审计）：在途期间重复点击忽略
let mbxSaveBusy = false;

async function mbxSaveAttachment(emailId, attachId, filename) {
    if (mbxSaveBusy) return;
    mbxSaveBusy = true;
    try {
        const dest = await window.__TAURI__.dialog.save({ defaultPath: filename });
        if (!dest) return;
        await htmx.ajax('POST', `/emails/${emailId}/attachments/${attachId}`, {
            target: '#toast',
            swap: 'innerHTML',
            values: { dest }
        });
    } catch (e) {
        console.error('[mbx] 保存对话框失败', e);
    } finally {
        mbxSaveBusy = false;
    }
}

// 数据目录迁移（M5）：系统对话框选目录 → POST 迁移（在线快照，重启生效）。
// 响应为存储 pane 片段：成功显示"待生效目录"，失败显示错误区。
// 请求护栏（M5 防抖审计）：弹窗+请求全程只允许一次在途
let mbxMoveBusy = false;

async function mbxMoveDataDir() {
    if (mbxMoveBusy) return;
    mbxMoveBusy = true;
    try {
        const dir = await window.__TAURI__.dialog.open({ directory: true });
        if (!dir) return;
        await htmx.ajax('POST', '/settings/storage/move', {
            target: '#settings-pane',
            swap: 'outerHTML',
            values: { dest: dir }
        });
    } catch (e) {
        console.error('[mbx] 打开目录对话框失败', e);
    } finally {
        mbxMoveBusy = false;
    }
}

// 向导页数据目录选择（与设置·存储页 mbxMoveDataDir 同源，仅选目录不迁移——
// 迁移由向导保存时按表单值处理）：选中后写回表单 input，用户仍可手改
async function mbxPickSetupDir(btn) {
    try {
        const dir = await window.__TAURI__.dialog.open({ directory: true });
        if (!dir) return;
        const input = btn.closest('.port-row')?.querySelector('input[name="data_dir"]');
        if (input) input.value = dir;
    } catch (e) {
        console.error('[mbx] 打开目录对话框失败', e);
    }
}

// ---------- 附件预览（M5）：img / pdf / text 三类内嵌渲染 ----------
// text 类服务端强制 text/plain 返回（防 text/html 脚本执行），前端 pre 渲染

function mbxPreviewAttachment(url, kind, name) {
    const modal = document.getElementById('preview-modal');
    const body = document.getElementById('pm-body');
    const nameEl = document.getElementById('pm-name');
    if (!modal || !body) return;
    if (nameEl) nameEl.textContent = name;
    body.innerHTML = '';
    if (kind === 'img') {
        const img = document.createElement('img');
        img.src = url;
        img.alt = name;
        body.appendChild(img);
    } else if (kind === 'pdf') {
        const frame = document.createElement('iframe');
        frame.src = url;
        frame.title = name;
        body.appendChild(frame);
    } else if (kind === 'text') {
        fetch(url)
            .then(r => r.ok ? r.text() : Promise.reject(new Error('HTTP ' + r.status)))
            .then(t => {
                const pre = document.createElement('pre');
                pre.className = 'pm-text';
                pre.textContent = t;
                body.appendChild(pre);
            })
            .catch(() => {
                body.textContent = '预览加载失败';
            });
    } else {
        // 未支持内嵌预览的格式：统一给出可预期反馈（M5 验收反馈）
        const tip = document.createElement('div');
        tip.className = 'pm-unsupported';
        tip.textContent = '该格式暂不支持内嵌预览，请使用"另存为"后用系统应用打开。';
        body.appendChild(tip);
    }
    modal.hidden = false;
}

function mbxClosePreview() {
    const modal = document.getElementById('preview-modal');
    if (!modal) return;
    modal.hidden = true;
    const body = document.getElementById('pm-body');
    if (body) body.innerHTML = '';  // 释放 img/iframe 资源
}

// Esc 关闭预览
document.addEventListener('keydown', ev => {
    if (ev.key === 'Escape') mbxClosePreview();
});

// ---------- 收取状态：按钮禁用 + 完成后整页刷新（单一通道） ----------
// 更新逻辑（需求二.2 简化）：状态片段轮询（收取中 2s / 空闲 10s）只更新自身文字；
// 后台收取结束时服务端下发一次 HX-Refresh 整页刷新。旧的 afterSwap 通道与
// 原生 fetch 10s 看门狗已删除（三通道去重为一）。
// data-running 标记由状态片段下发：收取中（含后台定时收取）禁用「收取新邮件」按钮。
function mbxSyncFetchButton() {
    const status = document.getElementById('fetch-status');
    const btn = document.getElementById('fetch-btn');
    if (!status || !btn) return;
    btn.disabled = status.getAttribute('data-running') === 'true';
}
document.body.addEventListener('htmx:afterSwap', mbxSyncFetchButton);
document.addEventListener('DOMContentLoaded', mbxSyncFetchButton);

// ---------- 撰写页：收件人/抄送联系人建议（模糊匹配姓名或地址） ----------
function mbxContactToken(inp) {
    const v = inp.value;
    const idx = Math.max(v.lastIndexOf(','), v.lastIndexOf(';'));
    return v.slice(idx + 1);
}

function mbxInitContacts() {
    document.querySelectorAll('input[name="to_addrs"], input[name="cc_addrs"]').forEach(inp => {
        if (inp.dataset.mbxContacts === '1') return;
        inp.dataset.mbxContacts = '1';
        inp.setAttribute('autocomplete', 'off');
        const host = inp.parentElement;
        if (host) host.style.position = 'relative';
        const box = document.createElement('div');
        box.className = 'contacts-pop';
        box.hidden = true;
        (host || document.body).appendChild(box);
        const state = { box, items: [], active: -1, timer: 0 };

        inp.addEventListener('input', () => {
            clearTimeout(state.timer);
            state.timer = setTimeout(() => mbxContactQuery(inp, state), 200);
        });
        inp.addEventListener('keydown', ev => mbxContactKey(inp, state, ev));
        inp.addEventListener('blur', () => setTimeout(() => { box.hidden = true; }, 150));
        inp.addEventListener('focus', () => { if (state.items.length) box.hidden = false; });
    });
}

async function mbxContactQuery(inp, state) {
    const q = mbxContactToken(inp).trim();
    if (!q) { state.box.hidden = true; state.items = []; return; }
    try {
        const r = await fetch('/contacts/suggest?q=' + encodeURIComponent(q), { cache: 'no-store' });
        if (!r.ok) return;
        state.items = await r.json();
        // 默认高亮首项：输入姓名后直接回车即可选中（无需先按 ↓），
        // 与「快速输入收件人」的目标一致；Esc/Ctrl 不影响输入
        state.active = state.items.length ? 0 : -1;
        if (!state.items.length) { state.box.hidden = true; return; }
        state.box.innerHTML = state.items.map((c, i) => {
            const name = c.name || c.addr;
            const addr = c.name ? `<span class="ci-addr">${escapeHtml(c.addr)}</span>` : '';
            return `<div class="contacts-item" data-i="${i}"><span class="ci-name">${escapeHtml(name)}</span>${addr}</div>`;
        }).join('');
        state.box.querySelectorAll('.contacts-item').forEach(el => {
            el.addEventListener('mousedown', ev => {
                ev.preventDefault();
                mbxContactPick(inp, state, Number(el.dataset.i));
            });
        });
        mbxContactMark(state);
        state.box.hidden = false;
    } catch (e) {
        // 建议失败不打扰输入
    }
}

function mbxContactPick(inp, state, i) {
    const c = state.items[i];
    if (!c) return;
    const v = inp.value;
    const idx = Math.max(v.lastIndexOf(','), v.lastIndexOf(';'));
    const head = idx >= 0 ? v.slice(0, idx + 1) : '';
    const token = c.name ? `${c.name} <${c.addr}>` : c.addr;
    inp.value = head + (head ? ' ' : '') + token + ', ';
    state.box.hidden = true;
    state.items = [];
    inp.dispatchEvent(new Event('input'));
    inp.focus();
}

function mbxContactKey(inp, state, ev) {
    if (state.box.hidden || !state.items.length) return;
    if (ev.key === 'ArrowDown') {
        ev.preventDefault();
        state.active = (state.active + 1) % state.items.length;
        mbxContactMark(state);
    } else if (ev.key === 'ArrowUp') {
        ev.preventDefault();
        state.active = (state.active - 1 + state.items.length) % state.items.length;
        mbxContactMark(state);
    } else if (ev.key === 'Enter') {
        if (state.active >= 0) { ev.preventDefault(); mbxContactPick(inp, state, state.active); }
    } else if (ev.key === 'Escape') {
        state.box.hidden = true;
    }
}

function mbxContactMark(state) {
    state.box.querySelectorAll('.contacts-item').forEach((el, i) => {
        el.classList.toggle('on', i === state.active);
    });
}

document.addEventListener('DOMContentLoaded', mbxInitContacts);

// ---------- 设置页 tab 高亮（stab 自身不重渲染，点击时同步 .on） ----------
document.addEventListener("click", function (e) {
    var stab = e.target.closest(".stab");
    if (!stab) { return; }
    document.querySelectorAll(".settings-tabs .stab").forEach(function (s) { s.classList.remove("on"); });
    stab.classList.add("on");
});
