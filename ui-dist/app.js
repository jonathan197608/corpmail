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

// 全部另存（A4）：选目录 → 服务端逐附件落盘（同名自动加序号，不覆盖）
async function mbxSaveAllAttachments(emailId) {
    if (mbxSaveBusy) return;
    mbxSaveBusy = true;
    try {
        const dir = await window.__TAURI__.dialog.open({ directory: true });
        if (!dir) return;
        await htmx.ajax('POST', `/emails/${emailId}/attachments/all`, {
            target: '#toast',
            swap: 'innerHTML',
            values: { dest_dir: dir }
        });
    } catch (e) {
        console.error('[mbx] 全部另存失败', e);
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

// ---------- 收取状态：按钮/设置入口禁用 + 完成后局部刷新 ----------
// 状态片段轮询（收取中 2s / 空闲 10s）只更新自身文字；后台收取结束时服务端
// 下发一次 HX-Trigger 事件 mbx:fetchDone，由此按当前页面做定向局部刷新。
// 不做整页 location.reload()：Tauri 自定义协议下 reload 行为不稳定，
// 且会丢失正在撰写的草稿与阅读位置。
// data-running 标记由状态片段下发：收取中（含后台定时收取）禁用「收取新邮件」
// 按钮与侧栏设置入口（避免拉取中误入设置改范围/重拉等造成竞态）。
function mbxSyncFetchButton() {
    const status = document.getElementById('fetch-status');
    const running = !!(status && status.getAttribute('data-running') === 'true');
    const btn = document.getElementById('fetch-btn');
    if (btn) btn.disabled = running;
    const gear = document.querySelector('a.settings-entry');
    if (gear) {
        gear.classList.toggle('disabled', running);
        gear.title = running ? '收取中，完成后可进入设置' : '设置';
    }
}
// click 兜底：即使 focus/键盘等绕过 pointer-events，仍阻止 disabled 齿轮跳转
document.body.addEventListener('click', ev => {
    const gear = ev.target.closest('a.settings-entry');
    if (gear && gear.classList.contains('disabled')) ev.preventDefault();
});
document.body.addEventListener('htmx:afterSwap', mbxSyncFetchButton);
document.addEventListener('DOMContentLoaded', mbxSyncFetchButton);

// ---------- 设置·收件同步：「重新拉取」仅在首次拉取范围变化时出现 ----------
// 服务端把已保存范围写在 data-first-fetch-initial；单选变化与之不一致才
// 显示按钮（防误触全量重拉）。保存/重拉返回的新片段经 afterSwap 重新校准。
function mbxRefetchToggle() {
    const row = document.querySelector('[data-first-fetch-initial]');
    const btn = document.getElementById('refetch-row');
    if (!row || !btn) return;
    const sel = row.querySelector('input[name="first_fetch_days"]:checked');
    btn.hidden = !sel || sel.value === row.dataset.firstFetchInitial;
}
document.body.addEventListener('change', ev => {
    if (ev.target && ev.target.name === 'first_fetch_days') mbxRefetchToggle();
});
document.body.addEventListener('htmx:afterSwap', mbxRefetchToggle);
document.addEventListener('DOMContentLoaded', mbxRefetchToggle);

// 后台收取完成：刷新当前页的动态区域（列表片段 + 侧栏计数），URL 与滚动位置不动
function mbxOnFetchDone() {
    // 1) 列表页：按当前地址（含未读/外部/搜索/页码）取列表片段换入
    // 列表页（含顶栏"共 N 封"）：#list-area 存在即列表页——非列表页
    // （详情/撰写/设置）没有该容器，自动跳过，不影响正在阅读/撰写的内容
    const list = document.getElementById('list-area');
    if (list) {
        htmx.ajax('GET', location.pathname + location.search, {
            target: '#list-area', swap: 'outerHTML'
        });
    }
    // 2) 侧栏（计数/高亮/状态条）：folder 取侧栏当前高亮项，避免非列表页丢失
    const on = document.querySelector('aside.side .folder.on');
    let folder = 'inbox';
    if (on) {
        const m = /folder=([a-z]+)/.exec(on.getAttribute('href') || '');
        if (m) folder = m[1];
    }
    const side = document.querySelector('aside.side');
    if (side) {
        htmx.ajax('GET', '/side/frame?folder=' + folder, {
            target: 'aside.side', swap: 'outerHTML'
        });
    }
}
document.body.addEventListener('mbx:fetchDone', mbxOnFetchDone);

// ---------- 星标翻转（M2-1）：mbx:starDone 事件携带新态 ----------
// 后端 POST /emails/{id}/star 空体 + 事件头；前端按所在页面分派：
// - 列表页：星标置顶会重排行序 → 重拉当前列表片段换入（保持过滤/页码，
//   与收取完成同路径；侧栏计数不随星标变，一并刷新无副作用）
// - 详情页：无重排，就地翻转按钮态（★/☆ 文案 + on 类 + aria-pressed）
document.body.addEventListener('mbx:starDone', function (ev) {
    var starred = !!(ev.detail && ev.detail.starred);
    if (document.getElementById('list-area')) {
        htmx.ajax('GET', location.pathname + location.search, {
            target: '#list-area', swap: 'outerHTML'
        });
        return;
    }
    var btn = document.querySelector('.star-btn[data-mail-id]');
    if (!btn) return;
    btn.classList.toggle('on', starred);
    btn.setAttribute('aria-pressed', starred ? 'true' : 'false');
    btn.title = starred ? '取消星标（不再置顶）' : '星标：置顶显示';
    // 详情页按钮带文字（列表页按钮仅 ★ 符号）、两形态统一一套文案
    if (btn.textContent.indexOf('星') >= 0) {
        btn.textContent = starred ? '★ 已星标' : '☆ 星标';
    } else {
        btn.textContent = '★';
    }
});

// ---------- 列表页：发送后瞬时反馈（M1-6）+ 撤销发送窗口（A1） ----------
// 发送跳回列表携带 just_sent=N（compose_send 排入撤销窗口时附加）：
// - 前 8 秒：toast 显示倒计时 + 「撤销发送」（POST /undo-send 回撰写页）
// - 倒计时结束：转为"邮件发送中…"，轮询 /emails/{id}/status 至终态：
//   sent → "已发送"（ok）2.5s 后淡出；failed → 错误文案（err）停留 6s
// - 终态后刷新列表区+侧栏（复用收取完成同一刷新路径），"发送中"行落定
// - 1.2s 间隔轮询；30s 未终态（SMTP 卡顿）不再打扰，提示稍后到发件箱查看
// URL 参数即刻清除（replaceState 不触发导航），刷新页面不会重复提示
(function () {
    var params = new URLSearchParams(location.search);
    var id = params.get('just_sent');
    if (!id || !document.getElementById('list-area')) return;
    params.delete('just_sent');
    var clean = params.toString();
    history.replaceState(null, '', location.pathname + (clean ? '?' + clean : ''));
    var slot = document.getElementById('toast');
    if (!slot) return;
    var UNDO_SECS = 8;
    var left = UNDO_SECS;
    var toast = document.createElement('div');
    toast.className = 'toast busy';
    var label = document.createElement('span');
    label.textContent = '邮件已进入发送队列（' + left + ' 秒内可撤销）';
    toast.appendChild(label);
    var undo = document.createElement('button');
    undo.type = 'button';
    undo.className = 'btn small toast-undo';
    undo.textContent = '撤销发送';
    undo.addEventListener('click', function () {
        undo.disabled = true;
        // htmx 会跟随 HX-Redirect 整页跳到撰写页（继续编辑）
        htmx.ajax('POST', '/emails/' + id + '/undo-send', { target: 'body', swap: 'none' });
    });
    toast.appendChild(undo);
    slot.appendChild(toast);
    var started = Date.now();
    var settle = function (text, cls, stayMs) {
        toast.className = 'toast ' + cls;
        toast.textContent = text;
        // 终态刷新：列表区（过滤/页码保持）+侧栏计数，复用收取完成路径
        if (window.mbxOnFetchDone) mbxOnFetchDone();
        setTimeout(function () { toast.remove(); }, stayMs);
    };
    var poll = function () {
        fetch('/emails/' + id + '/status', { cache: 'no-store' })
            .then(function (r) { return r.ok ? r.json() : null; })
            .then(function (s) {
                if (!s) return settle('发送状态查询失败，请在发件箱确认', 'err', 6000);
                if (s.status === 'sent') return settle('✓ 已发送', 'ok', 2500);
                if (s.status === 'failed') {
                    return settle('发送失败：' + (s.error || '未知原因'), 'err', 6000);
                }
                // queued/sending：继续轮询；超 30s 停止（SMTP 卡顿不动声色等太久）
                if (Date.now() - started > 30000) {
                    return settle('发送仍在进行，稍后可在发件箱确认', 'busy', 6000);
                }
                setTimeout(poll, 1200);
            })
            .catch(function () { return settle('网络异常，发送状态未知', 'err', 6000); });
    };
    // 撤销倒计时；到点后移除按钮并转入发送轮询
    var timer = setInterval(function () {
        left -= 1;
        if (left > 0) {
            label.textContent = '邮件已进入发送队列（' + left + ' 秒内可撤销）';
            return;
        }
        clearInterval(timer);
        if (undo.parentNode) undo.remove();
        label.textContent = '邮件发送中…';
        poll();
    }, 1000);
})();

// ---------- 撰写页：收件人/抄送联系人建议（模糊匹配姓名或地址） ----------
function mbxContactToken(inp) {
    const v = inp.value;
    const idx = Math.max(v.lastIndexOf(','), v.lastIndexOf(';'));
    return v.slice(idx + 1);
}

function mbxInitContacts() {
    // 密送框同样接入（M1-3 引入；与收/抄送同体验）
    document.querySelectorAll('input[name="to_addrs"], input[name="cc_addrs"], input[name="bcc_addrs"]').forEach(inp => {
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

// ---------- 向导"测试并保存"：进行中锁定表单，结束后恢复 ----------
// 双通道真实连接测试最长 ~15s（并行后）。htmx:configRequest触发时参数已收集完毕，
// 此处禁用 inputs 不影响本次提交序列化；afterRequest 无论成败都解锁，
// 保证失败重试（结果片段换入 #test-results 后表单必须可编辑）。
// 按钮禁用/恢复由 hx-disabled-elt 负责，此处只补文案与全表单锁定。
function mbxLockSetupForm(form, lock) {
    if (!form) return;
    form.querySelectorAll('input, select, textarea').forEach(function (el) {
        if (lock) {
            // 只标记"被我们禁用"的元素：天然 disabled（如有）解锁时不误开
            if (!el.disabled) {
                el.disabled = true;
                el.dataset.mbxLocked = '1';
            }
        } else if (el.dataset.mbxLocked) {
            el.disabled = false;
            delete el.dataset.mbxLocked;
        }
    });
    var back = form.querySelector('a.btn.link');
    if (back) back.classList.toggle('disabled', lock);
    var submit = form.querySelector('button[type=submit]');
    if (submit) {
        if (lock) {
            submit.dataset.mbxLabel = submit.textContent;
            submit.textContent = submit.dataset.busyLabel || '测试中…';
        } else if (submit.dataset.mbxLabel) {
            submit.textContent = submit.dataset.mbxLabel;
        }
    }
}

document.body.addEventListener('htmx:configRequest', function (ev) {
    var elt = ev.detail && ev.detail.elt;
    if (elt && elt.classList && elt.classList.contains('setup-card')) {
        mbxLockSetupForm(elt, true);
    }
});

document.body.addEventListener('htmx:afterRequest', function (ev) {
    var elt = ev.detail && ev.detail.elt;
    if (elt && elt.classList && elt.classList.contains('setup-card')) {
        mbxLockSetupForm(elt, false);
    }
});

// ---------- 撰写页：富文本编辑器（wangEditor 5，输出 HTML；纯文本只可单向升级） ----------
// 挂载/工具栏由编辑器渲染；提交时经 htmx:configRequest 把 getHtml() 注入参数。
var mbxEditor = null; // wangEditor 实例（富文本模式下存在）

function mbxReadInitialHtml() {
    const el = document.getElementById('rt-initial');
    return el ? el.value : '';
}

// 创建（或重建）编辑器：initial 为 HTML 字符串（已是净化后的安全内容）
function mbxBuildEditor(initial) {
    if (!window.wangEditor) return;
    // 升级/重建前销毁旧实例（editor 与 toolbar 成对销毁，防重复挂载）
    if (mbxEditor) {
        try { mbxEditor.destroy(); } catch (e) { /* 已销毁则忽略 */ }
        mbxEditor = null;
    }
    const { createEditor, createToolbar } = window.wangEditor;
    const editorConfig = {
        placeholder: '输入正文…（支持表格 / 颜色 / 列表 / 链接 / 图片）',
        MENU_CONF: {
            // 本地图片统一转 data URI 内嵌（企业内网无外置上传端点；
            // 保存/发送由后端净化管线只放行 data:image/*）
            uploadImage: {
                customUpload(file, insertFn) {
                    const reader = new FileReader();
                    reader.onload = () => insertFn(reader.result, file.name, '');
                    reader.readAsDataURL(file);
                }
            }
        }
    };
    try {
        mbxEditor = createEditor({
            selector: '#editor-container',
            html: initial,
            config: editorConfig,
            mode: 'default'
        });
        createToolbar({
            selector: '#toolbar-container',
            editor: mbxEditor,
            // 邮件场景不需要视频与全屏
            config: { excludeKeys: ['group-video', 'fullScreen'] },
            mode: 'default'
        });
    } catch (e) {
        console.error('编辑器初始化失败', e);
    }
}

// 纯文本 → 富文本（单向）：转义后按换行拆 <br> 作为初始内容；创建成功才切换显示
function mbxUpgradeHtml() {
    const ta = document.getElementById('body-text');
    const plainBox = document.getElementById('body-plain');
    const htmlBox = document.getElementById('body-html-box');
    const mode = document.getElementById('body-mode');
    let initial = '';
    if (ta) {
        const esc = ta.value
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;');
        initial = '<p>' + esc.replace(/\r?\n/g, '<br>') + '</p>';
    }
    mbxBuildEditor(initial);
    if (mbxEditor) {
        if (plainBox) plainBox.hidden = true;
        if (htmlBox) htmlBox.hidden = false;
        if (mode) mode.value = 'html';
    }
}

// htmx 发请求前注入编辑器当前 HTML，并执行"超大内嵌图片转附件"策略：
// ≥2MB 的 data:image 从正文剥离为 inline_images 参数（后端落为附件），
// 编辑器内替换为占位说明，防止 base64 撑爆单封体积。
// 注：不能靠 submit 事件刷新 hidden——htmx 的 submit 处理先注册
//（defer 顺序 htmx→app.js），后注册的监听来不及更新字段值。
var MBX_INLINE_IMG_LIMIT = 2 * 1024 * 1024;

function mbxExtractOversizedInlineImages(html) {
    var converted = [];
    var out = html.replace(/<img[^>]*src="(data:image\/[^"]+)"[^>]*>/gi, function (tag, uri) {
        // data:<mime>;base64,<payload>
        var m = uri.match(/^data:(image\/[a-z0-9.+-]+);base64,(.*)$/i);
        if (!m) return tag;
        var payload = m[2];
        // base64 长度 ≈ 字节 * 4/3
        var approxBytes = Math.floor(payload.length * 3 / 4);
        if (approxBytes < MBX_INLINE_IMG_LIMIT) return tag;
        var ext = (m[1].split('/')[1] || 'png').split('+')[0];
        converted.push({ name: 'image.' + ext, mime: m[1], data: payload });
        return '<p>〔图片超过 2MB，已转为附件：image.' + ext + '〕</p>';
    });
    return { html: out, converted: converted };
}

function mbxMigrateInlineImages() {
    if (mbxEditor) {
        var r = mbxExtractOversizedInlineImages(mbxEditor.getHtml());
        if (r.converted.length > 0) {
            mbxEditor.setHtml(r.html);
            return r.converted;
        }
    }
    return [];
}

// 提交归属表单判定：evt.elt 可能是 form（存草稿 submit）也可能是发送按钮本身
//（type=button + hx-include="closest form" 的 click 触发）。旧实现用
// elt.querySelector('#body-html')：button 的 querySelector 只查其自身子树，
// 找不到兄弟节点里的编辑器 hidden——发送路径 body_html 从未注入成功，
// HTML 转发邮件以空正文发出（2026-11-03 修复）。统一以 closest('form')
// 判定，两种触发路径等价。
function mbxComposeForm(evtElt) {
    if (!evtElt || typeof evtElt.closest !== 'function') return null;
    return evtElt.closest('form');
}

// wangEditor 输出的 <table> 无 border 属性（slate 模型不存边框）。
// 外发到企业客户端（Outlook/Foxmail）按属性画线，内联 style 可能被剥或
// 忽略——统一补 border="1"（HTML 属性形态兼容性最广）；已有 border 属性
// 则不动（转发场景源邮件的自带宽度/间距样式保持原样）。
// 净化器已放行 table 的 border 属性（ec3441b），此处补的属性可安全过净化
function mbxEnsureTableBorders(html) {
    return html.replace(/<table(?![^>]*\bborder\s*=)/gi, '<table border="1"');
}

document.body.addEventListener('htmx:configRequest', function (ev) {
    var detail = ev.detail || {};
    var form = mbxComposeForm(detail.elt);
    var hidden = document.getElementById('body-html');
    if (mbxEditor && form && hidden && form.contains(hidden) && detail.parameters) {
        var converted = mbxMigrateInlineImages();
        if (converted.length > 0) {
            detail.parameters['inline_images'] = JSON.stringify(converted);
            var note = document.querySelector('.compose-note-inline') || document.createElement('div');
            note.className = 'compose-note ok compose-note-inline';
            note.textContent = '✓ ' + converted.length + ' 张超过 2MB 的内嵌图片已转为附件，将随邮件发送';
            var card = document.querySelector('.compose-card');
            if (card) card.insertBefore(note, card.firstChild);
        }
        // 双保险：hidden.value 同步 + 参数显式覆盖（后者优先生效）。
        // 外发表格补 border=1（M1-5）：编辑器自建表格在收件方客户端才有分隔线
        hidden.value = mbxEnsureTableBorders(mbxEditor.getHtml());
        detail.parameters['body_html'] = hidden.value;
    }
});

// ---------- 撰写页：草稿自动保存（M3-1） ----------
// 停止输入 5s（防抖）→ 静默 POST /compose/autosave（与存草稿同一条保存路径，
// 返回轻量 JSON 无跳转）。首个 draft_id 回填隐藏域，此后自动保存变"覆盖"
// 不会堆积草稿；空表单后端守一道不落库。
// 请求经 htmx 发出：source=表单 使 htmx:configRequest 的 detail.elt 指向 form，
// 复用既有 wangEditor body_html/inline_images 注入（与存草稿完全同参形态）；
// handler 自定义接管响应（htmx 2.x ajax context 支持），无任何 swap 副作用。
(function () {
    var DELAY_MS = 5000;
    var timer = null;

    function composeForm() {
        var el = document.getElementById('autosave-hint');
        return el ? el.closest('form') : null;
    }

    function clock() {
        var t = new Date();
        function z(n) { return (n < 10 ? '0' : '') + n; }
        return z(t.getHours()) + ':' + z(t.getMinutes());
    }

    function trigger() {
        var form = composeForm();
        if (!form || !form.isConnected) return; // 已导航离开撰写页
        var values = {};
        try {
            new FormData(form).forEach(function (v, k) { values[k] = v; });
        } catch (e) { return; }
        htmx.ajax('POST', '/compose/autosave', {
            source: form,
            values: values,
            handler: function (_elt, info) {
                var xhr = info && info.xhr;
                if (!xhr || xhr.status !== 200) return;
                var res = null;
                try { res = JSON.parse(xhr.responseText); } catch (e) { return; }
                if (!res || !res.ok || res.draft_id <= 0) return;
                // 只有"本次新创建"才回填（编辑已有草稿时表单本来就有值；
                // 新建模板渲染 draft_id=0，与未创建等价）
                var idEl = form.querySelector('input[name="draft_id"]');
                if (idEl && (!idEl.value || idEl.value === '0')) {
                    idEl.value = String(res.draft_id);
                }
                var hint = document.getElementById('autosave-hint');
                if (hint) hint.textContent = '✓ 已自动保存 ' + clock();
            }
        });
        // 下轮防抖：保存后再有输入才会触发，不做周期性轮询
    }

    function schedule() {
        if (timer) clearTimeout(timer);
        timer = setTimeout(function () {
            timer = null;
            trigger();
        }, DELAY_MS);
    }

    document.body.addEventListener('input', function (ev) {
        var form = composeForm();
        if (!form || !form.contains(ev.target)) return;
        schedule();
    });
    // select 等不冒泡 input 的控件兜底
    document.body.addEventListener('change', function (ev) {
        var form = composeForm();
        if (!form || !form.contains(ev.target)) return;
        schedule();
    });
})();

// ---------- 撰写页：定时发送（M4-2） ----------
// 状态承载：#sched-at 隐藏域存 datetime-local 控件原始值（"YYYY-MM-DDTHH:MM"），
// 服务端 save_compose_form 统一解析成 epoch。两条发送路径同端点同表单：
// - 立即发送（button[hx-post="/compose/send"]:not(#sched-send-btn)）→ 点击即清空
//   hidden（capture 阶段抢在 htmx 收集参数之前）→ 提交 scheduled_at="" = 立即发
// - 定时发送（#sched-send-btn）→ htmx:configRequest 校验控件非空（空则拦请求
//   并提示），hidden 已由 change/input 同步为控件值 → 提交 = 定时
// 存草稿：hidden 原样提交（定时草稿保留计划；清空控件后存草稿 = 取消定时）
(function () {
    function hidden() { return document.getElementById('sched-at'); }
    function picker() { return document.getElementById('sched-input'); }
    function sync() {
        var h = hidden(), p = picker();
        if (h && p) h.value = p.value || '';
    }
    // 控件变化即同步隐藏域（change 覆盖清空/重选；input 覆盖手输部分）
    document.body.addEventListener('input', function (ev) {
        if (ev.target && ev.target.id === 'sched-input') sync();
    });
    document.body.addEventListener('change', function (ev) {
        if (ev.target && ev.target.id === 'sched-input') sync();
    });
    // 立即发送：capture 阶段清空（htmx 用冒泡监听，capture 先行——否则参数已收集）
    document.body.addEventListener('click', function (ev) {
        var t = ev.target;
        if (!t || !t.closest) return;
        var btn = t.closest('button[hx-post="/compose/send"]');
        if (btn && btn.id !== 'sched-send-btn') {
            var h = hidden();
            if (h) h.value = '';
        }
    }, true);
    // 定时发送：请求前校验控件已选（空则拦下并提示，模式同 M1-2 预校验）
    document.body.addEventListener('htmx:configRequest', function (ev) {
        var detail = ev.detail || {};
        var elt = detail.elt;
        if (!elt || !elt.matches || !elt.matches('#sched-send-btn')) return;
        var p = picker();
        if (!p || !p.value) {
            ev.preventDefault();
            var h = hidden();
            if (h) h.value = '';
            var note = document.createElement('div');
            note.className = 'compose-note err';
            note.textContent = '请先选择定时发送时刻（或点击「发送」立即发送）';
            var card = document.querySelector('.compose-card');
            if (card) card.insertBefore(note, card.firstChild);
            setTimeout(function () { if (note.parentNode) note.remove(); }, 4000);
        }
    });
})();

// ---------- 撰写页：发送前预校验（M1-2） ----------
// 挂 htmx:configRequest、精确匹配发送按钮（存草稿允许不完整内容，不校验）。
// preventDefault 即取消本次请求（htmx 在全部监听器执行完后统一检查），
// 此时 beforeRequest 未触达，hx-disabled-elt 不会留死锁禁用态。
// 校验顺序：无收件人（硬拦）→ 空正文（无附件硬拦 / 有附件确认）→ 无主题（确认）。
document.body.addEventListener('htmx:configRequest', function (ev) {
    var detail = ev.detail || {};
    var elt = detail.elt;
    if (!elt || !elt.matches || !elt.matches('button[hx-post="/compose/send"]')) return;
    var form = elt.closest('form');
    if (!form) return;
    // 每次校验先清旧错误条：校验失败插入新条，修正后重发则干净出发
    // （不放行态残留误导"仍未通过"），服务端错误条不受影响（无 .precheck 类）
    var card = form.closest('.compose-card');
    var oldNote = card && card.querySelector('.compose-note.precheck');
    if (oldNote) oldNote.remove();
    var field = function (name) {
        var el = form.querySelector('[name="' + name + '"]');
        return el ? String(el.value || '').trim() : '';
    };
    // 拦截：表单顶部错误条（.err 复用服务端错误样式；.precheck 供下次先清旧条）
    var fail = function (msg) {
        if (card) {
            var note = document.createElement('div');
            note.className = 'compose-note err precheck';
            note.textContent = msg;
            card.insertBefore(note, card.firstChild);
        }
        ev.preventDefault();
    };
    // 1) 收件人：to + cc + bcc 均空——SMTP 必拒，前端即时拦下，避免
    //    "存进草稿→入队→异步发失败"一轮往返才见错误（纯密送是合法形态）
    if (!field('to_addrs') && !field('cc_addrs') && !field('bcc_addrs')) {
        fail('请填写收件人、抄送或密送后再发送');
        return;
    }
    // 2) 正文：按当前模式取值。HTML 模式以 DOM 探针判定——wangEditor
    //    空内容 getHtml() 返回 <p><br></p>，textContent 为空即可判空；
    //    只有图片（无文字）视为有正文（贴图发送是完整语义）
    var modeEl = form.querySelector('#body-mode');
    var htmlMode = modeEl && modeEl.value === 'html' && mbxEditor;
    var emptyBody;
    if (htmlMode) {
        var probe = document.createElement('div');
        probe.innerHTML = mbxEditor.getHtml();
        emptyBody = (probe.textContent || '').trim() === '' && !probe.querySelector('img');
    } else {
        var ta = form.querySelector('#body-text');
        emptyBody = !ta || !(ta.value || '').trim();
    }
    if (emptyBody) {
        // 有附件/已保存附件时空正文是合法诉求（"只发附件"），降级为确认
        var kept = form.querySelector('#keep-ids');
        var hasAttach = pickedPaths.length > 0;
        if (!hasAttach && kept) {
            try { hasAttach = JSON.parse(kept.value || '[]').length > 0; } catch (e) { /* 空串按无附件 */ }
        }
        if (!hasAttach) {
            fail('正文不能为空；如只想发送附件，请在正文补充一句说明');
            return;
        }
        if (!window.confirm('正文为空，仅发送附件，确定吗？')) {
            ev.preventDefault();
            return;
        }
    }
    // 3) 主题为空：确认放行（企业邮件无主题易被过滤/忽视，确认防手滑）
    if (!field('subject') && !window.confirm('主题为空，确定直接发送吗？')) {
        ev.preventDefault();
    }
});

function mbxRtInit() {
    if (!document.getElementById('editor-container')) return;
    // 富文本模式（回复/转发 HTML 邮件、HTML 草稿）：直接挂载，初始内容
    // 来自服务端净化的 blockquote/正文；纯文本模式保持 textarea（升级按钮触发）
    const box = document.getElementById('body-html-box');
    if (box && !box.hidden) {
        mbxBuildEditor(mbxReadInitialHtml());
    }
}
document.addEventListener('DOMContentLoaded', mbxRtInit);

// ---------- 设置页保存按钮：配置无变化时禁用 ----------
// 进入/保存后按表单当前值建立基线；任一表单控件变化即启用保存，无修改则保持禁用。
// 排除 disabled（服务端只读字段）与 submit/button/file；password 空=保持不变，
// 清回空串视为无变化。sync 的开关改 hidden 值不走 input/change，需补 click 监听。
// 每次 swap 后经 htmx:afterSettle 重建基线（保存结果/切 Tab 均被覆盖）。
(function () {
    var base = typeof WeakMap !== 'undefined' ? new WeakMap() : null;
    function snapshot(form) {
        var out = {}, els = form.querySelectorAll('input, select, textarea');
        for (var i = 0; i < els.length; i++) {
            var el = els[i];
            if (el.disabled || !el.name) continue;
            var t = (el.type || '').toLowerCase();
            if (t === 'submit' || t === 'button' || t === 'file') continue;
            out[el.name] = t === 'radio' || t === 'checkbox'
                ? (el.checked ? el.value : '')
                : el.value;
        }
        return JSON.stringify(out);
    }
    function paneForm(node) {
        var f = node && node.closest ? node.closest('#settings-pane form') : null;
        return f;
    }
    function refresh(form) {
        if (!base || !form) return;
        var b = form.querySelector('button[type="submit"]');
        if (b) b.disabled = base.get(form) === snapshot(form);
    }
    function rebaseline(form) {
        if (!base || !form || !form.querySelector('button[type="submit"]')) return;
        base.set(form, snapshot(form));
        refresh(form);
    }
    document.addEventListener('input', function (ev) { refresh(paneForm(ev.target)); });
    document.addEventListener('change', function (ev) { refresh(paneForm(ev.target)); });
    document.addEventListener('click', function (ev) {
        var t = ev.target;
        var f = paneForm(t);
        if (f && t.classList && t.classList.contains('toggle')) refresh(f);
    });
    document.body.addEventListener('htmx:afterSettle', function (ev) {
        var pane = ev.detail && ev.detail.elt;
        var f = pane
            ? (pane.matches && pane.matches('#settings-pane form') ? pane
               : (pane.querySelector ? pane.querySelector('#settings-pane form') : null))
            : null;
        rebaseline(f);
    });
    document.addEventListener('DOMContentLoaded', function () {
        rebaseline(document.querySelector('#settings-pane form'));
    });
})();

// ---------- 列表批量操作（M1-1）：页内精确 + 跨页全选 ----------
// 状态机：
// - ids 模式：勾选的行 id 集合（页内）；
// - all 模式：表头全选触发跨页全选，exclude 为反勾选集合
//   （「选择全部 N 封」> 全部 N 封 · 排除 k 封）
// 提交两种形态：ids=[..] 或 select_all=1&exclude_ids=[..]（附列表筛选上下文）
// 操作后服务端回 mail_list 片段（#list-area 整段换入）+ mbx:fetchDone（边栏）
(function () {
    var stateBox = { mode: 'ids', ids: [], exclude: [], total: 0 };

    function val(id) {
        var el = document.getElementById(id);
        return el ? el.value : '';
    }
    function num(id) {
        return parseInt(val(id), 10) || 0;
    }
    // 当前筛选的检索态：检索场景禁用跨页全选（前后端口径一致）
    function isSearch() { return (val('ba-q') || '').trim() !== ''; }

    function bar() { return document.getElementById('batch-bar'); }
    function items() {
        return Array.prototype.slice.call(document.querySelectorAll('.mbx-item'));
    }

    function selectedCount() {
        if (stateBox.mode === 'all') {
            return Math.max(stateBox.total - stateBox.exclude.length, 0);
        }
        return stateBox.ids.length;
    }

    function render() {
        if (!bar()) return;
        var n = selectedCount();
        var countEl = document.getElementById('ba-count');
        var allLink = document.getElementById('ba-select-all');
        // 工具栏常驻：按选中数切换按钮禁用态（位置固定不跳动）
        var btns = document.querySelectorAll('#batch-bar [data-batch], #batch-bar #ba-clear');
        Array.prototype.forEach.call(btns, function (b) { b.disabled = n === 0; });
        if (!countEl) return;
        if (stateBox.mode === 'all') {
            var ex = stateBox.exclude.length;
            countEl.textContent = ex > 0
                ? '已选全部 ' + stateBox.total + ' 封 · 排除 ' + ex + ' 封'
                : '已选全部 ' + stateBox.total + ' 封';
            if (allLink) allLink.hidden = true;
        } else {
            countEl.textContent = n > 0 ? '已选 ' + n + ' 封' : '';
            // 跨页入口：非检索态即显示「选择全部 N 封」（总命中 = 筛选全集上限；
            // 单页亦显示——点即全选当前筛选全部，语义确定，用户不需记"有无下一页"）
            if (allLink) {
                allLink.hidden = isSearch() || n === 0;
                allLink.textContent = '选择全部 ' + stateBox.total + ' 封';
            }
        }
        // 表头三态
        var all = document.getElementById('batch-all');
        if (all) {
            var checked = items().filter(function (c) { return c.checked; }).length;
            all.checked = checked > 0 && checked === items().length;
            all.indeterminate = checked > 0 && checked < items().length;
        }
    }

    function resetSelection() {
        stateBox = { mode: 'ids', ids: [], exclude: [], total: num('ba-total') };
        items().forEach(function (c) { c.checked = false; });
        render();
    }

    function collectIds() {
        return items().filter(function (c) { return c.checked; })
            .map(function (c) { return parseInt(c.value, 10); });
    }

    function apply(action, confirmMsg) {
        var params = new URLSearchParams();
        params.set('action', action);
        params.set('folder', val('ba-folder') || 'inbox');
        params.set('unread', val('ba-unread') || '0');
        params.set('external', val('ba-external') || '0');
        var q = (val('ba-q') || '').trim();
        if (q) params.set('q', q);
        params.set('page', val('ba-page') || '1');
        // 每页条数（M4/紧凑化）：批量后回列表片段保持用户的分页规格
        var per = val('ba-per');
        if (per && per !== '10') params.set('per', per);
        // 排序（A2）：非默认才带，批量后列表片段保持当前排序
        var sort = val('ba-sort');
        if (sort && sort !== 'date_desc') params.set('sort', sort);
        if (stateBox.mode === 'all') {
            params.set('select_all', '1');
            params.set('exclude_ids', JSON.stringify(stateBox.exclude));
        } else {
            params.set('ids', JSON.stringify(stateBox.ids));
        }
        if (confirmMsg && !window.confirm(confirmMsg)) return;
        htmx.ajax('POST', '/emails/batch', {
            target: '#list-area', swap: 'outerHTML',
            values: Object.fromEntries(params.entries())
        });
    }

    function confirmText(action) {
        var n = selectedCount();
        if (action === 'delete') {
            return '确定删除选中的 ' + n + ' 封邮件？删除后无法恢复（含附件），草稿也会一并删除。';
        }
        return null;
    }

    document.body.addEventListener('change', function (ev) {
        var t = ev.target;
        if (!t.classList) return;
        if (t.id === 'batch-all') {
            if (t.checked) {
                // 表头全选：若跨页可用则提示入口，仍先做页内全选
                items().forEach(function (c) { c.checked = true; });
                stateBox.mode = 'ids';
                stateBox.ids = collectIds();
            } else {
                // 取消全选：若在跨页模式则整体退出，否则清页内
                resetSelection();
            }
            render();
            return;
        }
        if (t.classList.contains('mbx-item')) {
            var id = parseInt(t.value, 10);
            if (stateBox.mode === 'all') {
                // 跨页模式下行勾选 = 排除/撤销排除
                var idx = stateBox.exclude.indexOf(id);
                if (t.checked && idx >= 0) stateBox.exclude.splice(idx, 1);
                if (!t.checked && idx < 0) stateBox.exclude.push(id);
            } else {
                stateBox.ids = collectIds();
            }
            render();
        }
    });

    document.body.addEventListener('click', function (ev) {
        var t = ev.target;
        if (!t.closest) return;
        // 复选框位于行链接之外的独立 label 内（2026-11-03 结构修正）：
        // 原生勾选/原生 change 事件天然工作，无需任何 click 拦截
        var allLink = t.closest('#ba-select-all');
        if (allLink) {
            ev.preventDefault();
            stateBox.mode = 'all';
            stateBox.exclude = [];
            items().forEach(function (c) { c.checked = true; });
            render();
            return;
        }
        var clearBtn = t.closest('#ba-clear');
        if (clearBtn) {
            resetSelection();
            return;
        }
        var btn = t.closest('[data-batch]');
        if (btn) {
            var action = btn.getAttribute('data-batch');
            var sel = selectedCount();
            if (sel === 0) return;
            apply(action, confirmText(action));
        }
    });

    // 片段换入（首次加载/批量操作返回/收取刷新）重建基线
    function init() {
        if (!bar()) return;
        stateBox = { mode: 'ids', ids: [], exclude: [], total: num('ba-total') };
        render();
    }
    document.body.addEventListener('htmx:afterSwap', function (ev) {
        if (ev.detail && ev.detail.target && ev.detail.target.id === 'list-area') init();
    });
    document.addEventListener('DOMContentLoaded', init);
})();

// ---------- 键盘快捷键（A3） ----------
// 列表：j / ↓ 下移、k / ↑ 上移、Enter / o 打开选中、/ 聚焦搜索、n 新建邮件
// 详情：r 回复、f 转发、Esc 返回列表
// 输入框 / textarea / select / contenteditable 内的按键一律放行（Esc 仅失焦），
// 不打断打字；附件预览打开时 Esc 归预览模块，避免同时退出两层
(function () {
    var inField = function (el) {
        if (!el) return false;
        var tag = (el.tagName || '').toLowerCase();
        return tag === 'input' || tag === 'textarea' || tag === 'select' || el.isContentEditable;
    };
    var focusIdx = -1;
    var rows = function () {
        return Array.prototype.slice.call(document.querySelectorAll('#mail-list .mail-row'));
    };
    var paint = function () {
        var list = rows();
        if (focusIdx >= list.length) focusIdx = list.length - 1;
        list.forEach(function (r, i) { r.classList.toggle('kb-focus', i === focusIdx); });
        if (focusIdx >= 0 && list[focusIdx]) {
            list[focusIdx].scrollIntoView({ block: 'nearest' });
        }
    };
    document.addEventListener('keydown', function (ev) {
        if (ev.metaKey || ev.ctrlKey || ev.altKey) return;
        if (inField(ev.target)) {
            if (ev.key === 'Escape') ev.target.blur();
            return;
        }
        var modal = document.getElementById('preview-modal');
        var previewOpen = modal && !modal.hidden;
        if (ev.key === 'Escape') {
            if (previewOpen) return; // 预览层自己处理
            var back = document.querySelector('.topbar .back');
            if (back) { ev.preventDefault(); back.click(); }
            return;
        }
        var list = document.getElementById('mail-list');
        if (list) {
            var items = rows();
            if (!items.length) return;
            if (ev.key === 'j' || ev.key === 'ArrowDown') {
                ev.preventDefault();
                focusIdx = Math.min(items.length - 1, focusIdx + 1);
                paint();
            } else if (ev.key === 'k' || ev.key === 'ArrowUp') {
                ev.preventDefault();
                focusIdx = focusIdx < 0 ? 0 : Math.max(0, focusIdx - 1);
                paint();
            } else if (ev.key === 'Enter' || ev.key === 'o') {
                if (focusIdx < 0) { focusIdx = 0; paint(); return; }
                var link = items[focusIdx] && items[focusIdx].querySelector('.mr-link');
                if (link) { ev.preventDefault(); window.location.href = link.href; }
            } else if (ev.key === '/') {
                var box = document.querySelector('form.search input[name="q"]');
                if (box) { ev.preventDefault(); box.focus(); }
            } else if (ev.key === 'n') {
                ev.preventDefault();
                window.location.href = '/compose';
            }
            return;
        }
        var detail = document.querySelector('.mail-detail');
        if (detail) {
            if (ev.key === 'r') {
                var reply = detail.querySelector('a[href*="reply_to="]');
                if (reply) { ev.preventDefault(); window.location.href = reply.href; }
            } else if (ev.key === 'f') {
                var fwd = detail.querySelector('a[href*="forward_from="]');
                if (fwd) { ev.preventDefault(); window.location.href = fwd.href; }
            }
        }
    });
})();
