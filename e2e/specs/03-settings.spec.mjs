import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, openSplit, waitPaneVisible } from '../support/helpers.mjs'

// 03：设置页签 + 收件状态条 + 连接测试（E1–E5 / F1）
// 数据目录迁移（A6）已移到 95-migration：迁移成功会冻结整个进程的写入（R2），
// 必须放在主链路最末，否则其后的 04-07 全部写不进去
// 2026-11-16（T2/T3）：几何断言改 6 tab 循环、控件按 label[for] 取；
// 发信/收信互斥与关于页内容契约断言；tip 气泡交互改 hover（A16 去 tabindex）
describe('03 设置与收件状态', () => {
  it('发信页签：显示当前账号与凭据保管状态（不夹带收信内容）', async () => {
    await goto('/settings/send')
    await waitPaneVisible()
    const text = await $('body').getText()
    expect(text).toContain('e2e@corpmail.test')
    expect(text).toContain('发信（SMTP）')
    // T3：拆页契约——发信页不得夹带收信 pane 的任何痕迹（旧版两通道同页，
    // 拆分后串页=路由或模板回归）
    expect(text).not.toContain('收信（IMAP）')
    await expect($('button[hx-post="/settings/account/test-imap"]')).not.toBeExisting()
    await expect($('input[name="recv_host"]')).not.toBeExisting()
    await expect($('input[name="recv_port"]')).not.toBeExisting()
    await shot('设置-发信')
  })

  it('收信页签：IMAP 只读回显，测试收信入口与测试用密码输入在本页', async () => {
    await goto('/settings/receive')
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    const text = await $('#settings-pane').getText()
    expect(text).toContain('收信（IMAP）')
    expect(text).not.toContain('发信（SMTP）')
    await expect($('input[name="recv_host"]')).toBeExisting()
    await expect($('button[hx-post="/settings/account/test-imap"]')).toBeExisting()
    // A3：独立「测试用密码」输入存在且被 hx-include 并入（改密前先验证）。
    // 「仅用于本次测试」在 tip 的 data-tip/输入框 title 属性里（textContent 取不到）
    await expect($('input#recv-test-password[name="password"]')).toBeExisting()
    const inc = await $('button[hx-post="/settings/account/test-imap"]').getAttribute('hx-include')
    expect(inc).toBe('#recv-test-password')
    const tipMarked = await browser.execute(() => {
      const lab = document.querySelector('label[for="recv-test-password"]')
      const input = document.getElementById('recv-test-password')
      const tip = lab?.querySelector('[data-tip]')
      return {
        tipText: tip?.getAttribute('data-tip') ?? '',
        inputTitle: input?.getAttribute('title') ?? '',
      }
    })
    expect(tipMarked.tipText).toContain('仅用于本次测试收信，不保存')
    expect(tipMarked.inputTitle).toContain('留空则按已保存凭据测试')
    // 收信页没有可保存表单（唯一的 password 输入不在 form 内）
    expect(await $('#settings-pane form').isExisting()).toBe(false)
    await waitPaneVisible()
    await shot('设置-收信')
  })

  it('同步页签：间隔与首拉范围回显已保存值', async () => {
    await goto('/settings/sync')
    await expect($('input[name="sync_interval_min"]')).toBeExisting()
    await shot('设置-收件同步')
  })

  it('存储页签：数据目录与附件占用统计', async () => {
    await goto('/settings/storage')
    const text = await $('body').getText()
    expect(text).toContain('数据与附件')
    expect(await $('.dir-path').isExisting()).toBe(true)
    await shot('设置-存储管理')
  })

  it('T3 关于页：产品定位、语义版本与隐私承诺', async () => {
    await goto('/settings/about')
    await waitBoot()
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    await waitPaneVisible()
    const text = await $('#settings-pane').getText()
    expect(text).toContain('关于')
    expect(text).toContain('企业邮箱本地客户端')
    // 版本号 vX.Y.Z（app_version 注入；e2e 二进制按 Cargo.toml 0.3.0）
    expect(text).toMatch(/v\d+\.\d+\.\d+/)
    // 隐私承诺与零遥测（上架口径）
    expect(text).toContain('不包含广告或遥测组件')
    expect(text).toContain('隐私承诺')
    // 上架合规：不泄露技术栈（框架/存储引擎）
    expect(text).not.toContain('Tauri')
    expect(text).not.toContain('SQLite')
    await shot('设置-关于')
  })

  it('T3 旧路由别名：/settings/account 与 /settings?tab=account 都落到发信页', async () => {
    // 「账号」是拆分前旧 tab 名，保留别名的契约：归一到 send、历史书签不 404
    await goto('/settings/account')
    await waitBoot()
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    await waitPaneVisible()
    let sel = await browser.execute(() =>
      document.querySelector('.settings-tabs .stab[aria-selected="true"]')?.textContent)
    expect(sel).toBe('发信')
    expect(await $('#settings-pane').getText()).toContain('发信（SMTP）')

    await goto('/settings?tab=account')
    await waitBoot()
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    await waitPaneVisible()
    sel = await browser.execute(() =>
      document.querySelector('.settings-tabs .stab[aria-selected="true"]')?.textContent)
    expect(sel).toBe('发信')
    await shot('设置-账号别名归一发信')
  })

  it('E3 连接测试按钮：SMTP/IMAP 双通道打真 mock 显示结果', async () => {
    await goto('/settings/send')
    const smtpBtn = await $('button[hx-post="/settings/account/test-smtp"]')
    await smtpBtn.click()
    await browser.waitUntil(
      async () => {
        const t = await $('#smtp-result').getText()
        return t.length > 0
      },
      { timeout: 30000, timeoutMsg: 'SMTP 测试结果未返回' },
    )
    const smtpText = await $('#smtp-result').getText()
    expect(smtpText).toContain('发信通道')
    expect(smtpText).not.toContain('失败')
    await shot('设置-SMTP测试通过')

    // 收信测试已拆到独立页：同一页不再同时存在两个通道的按钮
    await goto('/settings/receive')
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    // A3：填一个测试密码走「未保存新密码也可验通道」路径（mock 接受任意凭据）
    const pw = await $('input#recv-test-password')
    if (await pw.isExisting()) await pw.setValue('e2e-new-pw')
    await $('button[hx-post="/settings/account/test-imap"]').click()
    await browser.waitUntil(
      async () => (await $('#imap-result').getText()).length > 0,
      { timeout: 30000, timeoutMsg: 'IMAP 测试结果未返回' },
    )
    const imapText = await $('#imap-result').getText()
    expect(imapText).toContain('收信通道')
    expect(imapText).not.toContain('失败')
    await shot('设置-IMAP测试通过')
  })

  it('T2 设置表单固定宽度：六个 pane 同宽；.sfield 页标签/控件各对齐同线', async () => {
    const widths = []
    for (const tab of ['send', 'receive', 'sync', 'storage', 'templates', 'about']) {
      await goto(`/settings/${tab}`)
      await $('#settings-pane').waitForExist({ timeout: 20000 })
      await waitPaneVisible()
      widths.push(
        await browser.execute(() =>
          Math.round(document.getElementById('settings-pane').getBoundingClientRect().width),
        ),
      )
    }
    // 断言各用例（waitPaneVisible 后的稳定 DOM）不能省；同宽 + ≤760
    expect(widths.every((w) => w === widths[0])).toBe(true)
    expect(widths[0]).toBeLessThanOrEqual(760)
    expect(widths[0]).toBeGreaterThan(0)

    await goto('/settings/send')
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    await waitPaneVisible()
    const m = await browser.execute(() => {
      const declared = getComputedStyle(document.documentElement)
        .getPropertyValue('--settings-form-width').trim()
      const pane = document.getElementById('settings-pane')
      const fields = [...pane.querySelectorAll('.sfield')]
      // T2：控件位置一律按 label[for] 指向的控件取——lastElementChild 在
      // tip/hidden input 混入时取到错误元素（T3 契约：每个 .sfield 都有关联）。
      // 复合控件（.port-row 双格 host:port）量容器右缘：右格 port 收格对齐，
      // 单量 host 会得出比全宽控件短的假右缘
      const controlOf = (f) => {
        const lab = f.querySelector('label[for]')
        if (!lab) return null
        const c = document.getElementById(lab.getAttribute('for'))
        if (!c) return null
        return c.closest('.port-row') || c.closest('.radio-row') || c
      }
      const rights = [...new Set(fields.map((f) => {
        const c = controlOf(f)
        return c ? Math.round(c.getBoundingClientRect().right) : null
      }).filter((x) => x !== null))]
      const lefts = [...new Set(fields.map((f) =>
        Math.round(f.querySelector('label').getBoundingClientRect().left)))]
      const actions = pane.querySelector('.settings-actions')
      const save = actions && actions.querySelector('button[type="submit"]')
      const firstCol = fields.length
        ? getComputedStyle(fields[0]).gridTemplateColumns.split(' ')[0]
        : null
      const s = document.querySelector('.settings')
      const cs = getComputedStyle(s)
      const avail = s.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight)
      return {
        declared,
        width: Math.round(pane.getBoundingClientRect().width),
        expected: Math.min(760, Math.round(avail)),
        rights,
        lefts,
        fieldCount: fields.length,
        firstCol,
        saveRight: save ? Math.round(save.getBoundingClientRect().right) : null,
        justify: actions ? getComputedStyle(actions).justifyContent : null,
        // R10：tip 必须留在 label 内（A16 只去交互属性，不移 DOM 位置）
        tipsInLabel: fields.reduce(
          (n, f) => n + f.querySelectorAll('label [data-tip]').length, 0),
        tipsTotal: pane.querySelectorAll('.sfield [data-tip]').length,
      }
    })
    expect(m.tipsInLabel).toBe(m.tipsTotal) // tip 全在 label 内，未被移出到控件列
    expect(m.declared).toBe('760px')
    expect(m.width).toBe(m.expected)
    expect(m.fieldCount).toBeGreaterThanOrEqual(1)
    expect(m.lefts.length).toBe(1) // 所有标签左边缘同线
    expect(m.rights.length).toBe(1) // 所有控件右边缘同线（label[for] 口径）
    expect(m.saveRight).toBe(m.rights[0]) // 动作行也收在控件右边缘
    expect(m.justify).toBe('flex-end')
    expect(m.firstCol).toBe('148px') // V8 静态护栏的 E2E 面

    // receive / sync：同契约逐页（labels 左线、控件右线、动作行右对齐）。
    // templates 不入此循环：其「内容」.sfield 是跨列布局（textarea 占满
    // 表单整行，右缘 = settings-card 内容右缘，与双列控件线天然不同线）
    // ——单独用跨列断言（下方 stack）+ 动作行对齐
    const paneMetrics = {}
    for (const tab of ['receive', 'sync']) {
      await goto(`/settings/${tab}`)
      await waitPaneVisible()
      paneMetrics[tab] = await browser.execute(() => {
        const pane = document.getElementById('settings-pane')
        const fields = [...pane.querySelectorAll('.sfield')]
        const controlOf = (f) => {
          const lab = f.querySelector('label[for]')
          if (!lab) return null
          const c = document.getElementById(lab.getAttribute('for'))
          if (!c) return null
          // 复合控件量容器（port-row / radio-row），单个子控件右缘不落线
          return c.closest('.port-row') || c.closest('.radio-row') || c
        }
        const rights = [...new Set(fields.map((f) => {
          const c = controlOf(f)
          return c ? Math.round(c.getBoundingClientRect().right) : null
        }).filter((x) => x !== null))]
        const lefts = [...new Set(fields.map((f) =>
          Math.round(f.querySelector('label').getBoundingClientRect().left)))]
        const actions = pane.querySelector('.settings-actions')
        const save = actions && actions.querySelector('button[type="submit"]')
        return {
          fieldCount: fields.length,
          rights,
          lefts,
          saveRight: save ? Math.round(save.getBoundingClientRect().right) : null,
          justify: actions ? getComputedStyle(actions).justifyContent : null,
          firstCol: fields.length
            ? getComputedStyle(fields[0]).gridTemplateColumns.split(' ')[0]
            : null,
        }
      })
      const p = paneMetrics[tab]
      expect(p.fieldCount).toBeGreaterThanOrEqual(1)
      expect(p.lefts.length).toBe(1)
      expect(p.rights.length).toBe(1)
      // receive 页无保存表单：只断右对齐的控件线本身
      if (p.saveRight !== null) {
        expect(p.saveRight).toBe(p.rights[0]) // 断言失败时 p 整体在下方补吐
      }
      expect(p.justify === null || p.justify === 'flex-end').toBe(true)
      expect(p.firstCol).toBe('148px')
    }

    // templates：跨列形态契约——首列仍 148px、动作行 flex-end、
    // 内容控件占满整行（右缘 = pane 内容右缘）
    await goto('/settings/templates')
    await waitPaneVisible()
    const tpl = await browser.execute(() => {
      const pane = document.getElementById('settings-pane')
      const fields = [...pane.querySelectorAll('.sfield')]
      const xfield = pane.querySelector('.sfield[data-tpl-field]')
      const lab = xfield.querySelector('label[for]')
      const c = document.getElementById(lab.getAttribute('for'))
      // templates 的提交按钮（「新增模板」）不在 .settings-actions 容器里
      //（模板页是 list+form 混排，动作行容器属 settings 卡片形态）
      const save = pane.querySelector('button[type="submit"]')
      // 内容基准量 .tpl-form 右缘而非 #settings-pane：form 落在 .settings-card
      // 内，card 有 16px 右内距 + 1px 边框，pane 内容右缘比表单行宽 17px，
      // 量 pane 会把贴齐表单行的合法布局误判为不满行（第 13 轮失败根因）
      const form = pane.querySelector('.tpl-form')
      const rect = c.getBoundingClientRect()
      return {
        fieldCount: fields.length,
        firstCol: getComputedStyle(fields[0]).gridTemplateColumns.split(' ')[0],
        controlRight: Math.round(rect.right),
        formRight: form ? Math.round(form.getBoundingClientRect().right) : null,
        labelBottom: Math.round(lab.getBoundingClientRect().bottom),
        controlTop: Math.round(rect.top),
        saveRight: save ? Math.round(save.getBoundingClientRect().right) : null,
      }
    })
    expect(tpl.fieldCount).toBeGreaterThanOrEqual(2)
    expect(tpl.firstCol).toBe('148px')
    // 跨列控件占满表单行宽：textarea 右缘与 form 右缘一致（4px 容差排版舍入）
    expect(tpl.controlRight).toBeGreaterThanOrEqual(tpl.formRight - 4)
    // 提交按钮收在表单行右缘（T2 修复：CSS margin-left:auto，与其他页动作行同线）
    expect(tpl.saveRight).toBeGreaterThanOrEqual(tpl.formRight - 4)
    // label 一行、控件换行贴其下
    expect(tpl.labelBottom).toBeLessThanOrEqual(tpl.controlTop + 2)
    await shot('设置-固定宽度与对齐')
  })

  it('E2b 同步间隔保存回显：改 5 分钟提交 → pane 回显 5', async () => {
    await goto('/settings/sync')
    await $('input[name="sync_interval_min"]').setValue('5')
    await $('form button[type="submit"]').click()
    await browser.waitUntil(
      async () => (await $('input[name="sync_interval_min"]').getValue()) === '5',
      { timeout: 15000, timeoutMsg: '保存后间隔未回显新值' },
    )
    await shot('设置-同步间隔已保存')
  })

  it('E6 字段说明气泡：hover 显示、Esc 关闭（A16 去 tabindex 后的交互契约）', async () => {
    // 2026-11-10：label 后 hint 小字改气泡。A16（2026-11-16）tip 摘掉
    // tabindex/role=button（label 内不允许交互元素嵌套）——键盘路径由
    // tip-of-label 的整体朗读承担，气泡触发收敛到 hover/click 委托
    await goto('/settings/sync')
    await waitBoot()
    const state = await browser.execute(() => {
      const t = document.querySelector('.tip-i[data-tip]')
      if (!t) return null
      // hover 委托（mouseover → tipOf → show）：focus 路径已随 tabindex 一并移除
      t.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }))
      const layer = document.querySelector('.tip-layer')
      return {
        tip: t.getAttribute('data-tip'),
        visible: !!layer && !layer.hidden,
        text: layer ? layer.textContent : '',
      }
    })
    expect(state && state.tip).toBeTruthy()
    expect(state.visible).toBe(true)
    expect(state.text).toBe(state.tip)
    await shot('设置-气泡说明悬停显示')

    await browser.execute(() =>
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })),
    )
    const closed = await browser.execute(() => {
      const l = document.querySelector('.tip-layer')
      return !l || l.hidden
    })
    expect(closed).toBe(true)
  })

  it('T3 六个 tab 键盘可达：Tab 键序 + Enter 激活切换 pane（A15）', async () => {
    // A15：tab 由 span 改 button[role=tab]——focus() 后 Enter 应切换 pane。
    // 选第 2 个 tab（收信），Enter 后断言 pane 内容换成收信页
    await goto('/settings/send')
    await waitBoot()
    await $('#settings-pane').waitForExist({ timeout: 20000 })
    await waitPaneVisible()
    await browser.execute(() => {
      const tabs = document.querySelectorAll('.settings-tabs .stab')
      tabs[1].focus()
    })
    await browser.keys('Enter')
    await browser.waitUntil(
      async () => (await $('#settings-pane').getText()).includes('收信（IMAP）'),
      { timeout: 10000, timeoutMsg: '键盘 Enter 未切换到收信 pane',
      },
    )
    const sel = await browser.execute(() =>
      document.querySelector('.settings-tabs .stab[aria-selected="true"]')?.textContent)
    expect(sel).toBe('收信')
    // Space 激活（APG tab 模式另一键面，A15 keydown 委托同段实现）：
    // focus 第 3 个 tab，空格切到收件同步
    await browser.execute(() => {
      document.querySelectorAll('.settings-tabs .stab')[2].focus()
    })
    await browser.keys('Space')
    await browser.waitUntil(
      async () => (await $('#settings-pane').getText()).includes('收件同步'),
      { timeout: 10000, timeoutMsg: '键盘 Space 未切换到收件同步 pane',
      },
    )
    const sel2 = await browser.execute(() =>
      document.querySelector('.settings-tabs .stab[aria-selected="true"]')?.textContent)
    expect(sel2).toBe('收件同步')
    await shot('设置-键盘Enter切tab')
  })

  it('收取新邮件：状态条完成「运行/空闲」状态转变、阶段 DOM 值有效（无错误）', async () => {
    // T5/R7：用 MutationObserver 记录 data-running==='true' **曾出现**，再断
    // 终态回 false——两段合起来才是「运行→空闲」。旧断言第一段接受空闲文案，
    // 坏实现（根本不置 true）会假通过
    await goto('/')
    await waitBoot()
    // 2026-11 改版：「收取新邮件」收进顶栏 ⋯ 菜单（.tb-more），先展开再点
    await openSplit('.tb-more [data-split-toggle]')
    // 点击前装观察器：状态片段由 htmx 换入，running 可能极短，轮询会错过
    await browser.execute(() => {
      const bar = document.getElementById('fetch-status')
      window.__FETCH_RAN__ = !!(bar && bar.dataset.running === 'true')
      const obs = new MutationObserver(() => {
        const b = document.getElementById('fetch-status')
        if (b && b.dataset.running === 'true') window.__FETCH_RAN__ = true
      })
      obs.observe(document.body, { childList: true, subtree: true, attributes: true,
        attributeFilter: ['data-running'] })
      window.__FETCH_OBS__ = obs
    })
    await $('#fetch-btn').click()
    // 第一段：进入运行态或直接空转完成（文案层兜底，running 的真实性由观察器断）
    await browser.waitUntil(
      async () => {
        const bar = await $('#fetch-status')
        const t = (await bar.isExisting()) ? await bar.getText() : ''
        return t.includes('正在收取') || t.includes('没有新邮件') || t.includes('新收')
      },
      { timeout: 60000, timeoutMsg: '状态条未进入已知状态' },
    )
    await shot('收件状态条')
    // 第二段：终态必须回到空闲（data-running=false）且阶段文案有效
    await browser.waitUntil(
      async () => browser.execute(() =>
        document.getElementById('fetch-status')?.dataset.running === 'false'),
      { timeout: 60000, timeoutMsg: '状态条未回到空闲态（data-running 恒 true）' },
    )
    // R7：running 必须真的出现过——否则「空闲文案 + 终态 false」会让坏实现
    // 假通过（从不置 running=true 也能过）。观察器在点击前已装好，此刻读标志
    const ran = await browser.execute(() => {
      const seen = window.__FETCH_RAN__ === true
      if (window.__FETCH_OBS__) { window.__FETCH_OBS__.disconnect(); window.__FETCH_OBS__ = null }
      return seen
    })
    expect(ran).toBe(true, '收取期间 data-running 必须曾为 true（运行→空闲两段式）')
    const idle = await browser.execute(() => {
      const el = document.getElementById('fetch-status')
      return {
        text: el?.textContent ?? '',
        hasErr: !!el?.querySelector('.fs-err'),
        hasNew: !!el?.querySelector('.fs-new'),
        hasDim: !!el?.querySelector('.fs-dim'),
      }
    })
    // 阶段 DOM 值：空闲三形态之一（fs-err 被下方负面断言排除）
    expect(idle.hasErr || idle.hasNew || idle.hasDim).toBe(true)
    expect(idle.text).not.toContain('凭据缺失')
    expect(idle.text).not.toContain('超时')
    expect(idle.hasErr).toBe(false)
    await shot('收件状态条-回到空闲')
  })
})
