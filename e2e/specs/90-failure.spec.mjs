import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, htmxAjax, runDir } from '../support/helpers.mjs'
import fs from 'node:fs'
import path from 'node:path'

// 90：失败链路，独立于成功链路（00–07），按文件名顺序最后执行。
// 注入手段全部外部可控，不改应用配置、不依赖 mock 停机：
//   - mock 对 fail@ 开头收件人返回 550（发送走完整 failed 状态机）
//   - 不可达端口 / 空表单字段 / 非法迁移目标（htmx.ajax 直接 POST）
// 前置：成功链路已通过 01 配置并入信；本文件只在配置后的状态上验证
// 「失败必须被拒、状态必须可恢复」。

describe('90 失败链路', () => {
  it('F1 向导空字段提交：被拦截提示，不跳转完成页', async () => {
    await goto('/welcome/setup')
    await waitBoot()
    const res = await htmxAjax(
      'POST',
      '/welcome/setup/save',
      { account_prefix: '', password: '' },
      '#test-results',
    )
    expect(res.text).toContain('请填写邮箱账号与密码')
    expect(res.redirect).toBeNull()
    await shot('失败-向导空字段拦截')
  })

  it('F2 向导双通道失败（不可达端口）：未保存 + 已有配置不被覆盖', async () => {
    await goto('/welcome/setup')
    await waitBoot()
    const res = await htmxAjax(
      'POST',
      '/welcome/setup/save',
      {
        account_prefix: 'e2e',
        account_domain: 'corpmail.test',
        password: 'wrong-pw',
        display_name: '失败注入',
        smtp_host: '127.0.0.1',
        smtp_port: '1',
        smtp_security: 'plain',
        recv_host: '127.0.0.1',
        recv_port: '1',
        recv_security: 'plain',
      },
      '#test-results',
    )
    expect(res.text).toContain('未保存')
    expect(res.redirect).toBeNull()
    // 配置未被破坏：主列表仍可访问且有邮件（收发件端口/账号原样保留）
    await goto('/')
    await waitBoot()
    expect((await $$('.mail-row')).length).toBeGreaterThan(0)
    await shot('失败-通道未保存且配置完好')
  })

  it('F3 迁移失败：空目标 / 同当前目录 / 目标已有库，均报错不切换', async () => {
    await goto('/settings/storage')
    await waitBoot()
    const cur = await $('.dir-path').getAttribute('title')
    expect(cur).toBeTruthy()

    // 目标已有库（与单测 ui_router.rs:556 同场景）
    const occupied = path.join(runDir(), 'occupied-data')
    fs.mkdirSync(occupied, { recursive: true })
    fs.writeFileSync(path.join(occupied, 'corpmail.db'), 'not a real db')

    let res = await htmxAjax('POST', '/settings/storage/move', { dest: '' }, '#settings-pane')
    expect(res.text).toContain('未选择目标目录')

    res = await htmxAjax('POST', '/settings/storage/move', { dest: cur }, '#settings-pane')
    expect(res.text).toContain('相同')

    res = await htmxAjax('POST', '/settings/storage/move', { dest: occupied }, '#settings-pane')
    expect(res.text).toContain('corpmail.db')
    // 当前目录不变（仍指向 cur，未被非法目标污染）
    expect(await $('.dir-path').getAttribute('title')).toBe(cur)
    await shot('失败-迁移三连拒')
  })

  it('F4 发送失败链路：fail@ 550 拒收 → failed 状态 → 重发回撰写页', async () => {
    await goto('/compose')
    await waitBoot()
    // 发送走 htmxAjax 而非点按钮：跳回列表后 app.js 首帧 replaceState 清掉
    // just_sent（防刷新重复提示），URL 里取不到 id；htmxAjax 直接读响应头
    // HX-Redirect（在任何前端清理之前），确定性拿到邮件 id
    const res = await htmxAjax('POST', '/compose/send', {
      to_addrs: 'fail@corpmail.test',
      subject: 'E2E 发送失败注入',
      body_text: 'mock SMTP 对该收件人返回 550，应走完整 failed 状态机。',
      body_mode: 'plain',
      scheduled_at: '',
    })
    expect(res.status).toBe(200)
    expect(res.redirect).toContain('just_sent=')
    const id = new URL(res.redirect, 'http://x/').searchParams.get('just_sent')
    expect(id).toBeTruthy()
    await shot('失败-已入队撤销窗')

    // 撤销窗 8s + 调度线程派发 + SMTP 550 → status=failed，error 落库
    await browser.waitUntil(
      async () => {
        await goto(`/emails/${id}`)
        return (await $('body').getText()).includes('发送失败')
      },
      { timeout: 45000, interval: 3000, timeoutMsg: '45s 内未进入 failed 状态（查 send worker 派发）' },
    )
    await waitBoot()
    const status = await $('.md-status.failed').getText()
    expect(status).toContain('发送失败')
    await shot('失败-详情页failed状态')

    // 重发入口：详情页按钮 → 回撰写页（预填主题）
    const resendBtn = await $('button[hx-post*="/resend"]')
    await expect(resendBtn).toBeExisting()
    await resendBtn.click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('/compose'),
      { timeoutMsg: '重发未回撰写页' },
    )
    await waitBoot()
    expect(await $('input[name="subject"]').getValue()).toContain('E2E 发送失败注入')
    await shot('失败-重发回撰写页')
  })
})
