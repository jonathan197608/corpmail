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

// ---------- 后台收取完成 → 整页自动刷新（看门狗，双通道兜底） ----------
// 依赖链「htmx 轮询 → just_finished 消费 → HX-Refresh 头 → reload」中，
// htmx 轮询元素靠响应自替换延续触发器，任一环节断裂（触发器未重建/请求未
// 发出）都会让刷新永不发生。这里用两条独立通道兜底，服务端在消费 just_finished
// 时同时下发 HX-Refresh 头与片段根节点 data-just-finished 标记：
// 通道1 afterSwap：htmx 轮询响应换入的片段若带标记，立即整页刷新（不依赖响应头）；
// 通道2 原生 fetch 每 10s 轮询：htmx 请求本身未发出时由此检测（fetch 对自定义
//      协议可用——附件预览 mbxPreviewAttachment 已验证）
document.body.addEventListener('htmx:afterSwap', () => {
    const el = document.getElementById('fetch-status');
    if (el && el.hasAttribute('data-just-finished')) location.reload();
});

(function fetchRefreshWatchdog() {
    if (!document.getElementById('fetch-status')) return; // 欢迎/向导页无状态条，不启动
    const TICK_MS = 10000;
    setTimeout(async function poll() {
        try {
            const r = await fetch('/emails/fetch/status', { cache: 'no-store' });
            const t = await r.text();
            if (r.headers.get('HX-Refresh') === 'true' || t.includes('data-just-finished')) {
                location.reload();
                return; // 刷新后本轮看门狗终止，新页面重新启动
            }
        } catch (e) { /* 本地协议异常：下轮再试 */ }
        setTimeout(poll, TICK_MS);
    }, TICK_MS);
})();

// ---------- 设置页 tab 高亮（stab 自身不重渲染，点击时同步 .on） ----------
document.addEventListener("click", function (e) {
    var stab = e.target.closest(".stab");
    if (!stab) { return; }
    document.querySelectorAll(".settings-tabs .stab").forEach(function (s) { s.classList.remove("on"); });
    stab.classList.add("on");
});
