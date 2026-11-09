import { browser, $, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText, htmxAjax, runDir, openSplit } from '../support/helpers.mjs'
import path from 'node:path'
import fs from 'node:fs'

// 03：设置页签 + 收件状态条 + 连接测试 + 数据目录迁移（E1–E5 / A6 / F1）
describe('03 设置与收件状态', () => {
  it('账号页签：显示当前账号与凭据保管状态', async () => {
    await goto('/settings')
    const text = await $('body').getText()
    expect(text).toContain('e2e@corpmail.test')
    await shot('设置-账号')
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

  it('E3 连接测试按钮：SMTP/IMAP 双通道打真 mock 显示结果', async () => {
    await goto('/settings/account')
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

  it('A6 数据目录迁移：POST move → 待生效目录 + 重启徽标', async () => {
    await goto('/settings/storage')
    const dest = path.join(runDir(), 'moved-data')
    fs.mkdirSync(dest, { recursive: true })
    const res = await htmxAjax('POST', '/settings/storage/move', { dest }, '#settings-pane')
    // 单测钉过的文案（ui_router.rs:530）：徽标为「重启后生效」+ 旧目录「当前使用」注记
    expect(res.text).toContain('重启后生效')
    await waitText('#settings-pane', '当前使用')
    await shot('设置-数据目录待生效')
  })

  it('关于页签', async () => {
    await goto('/settings/about')
    await shot('设置-关于')
  })

  it('收取新邮件：顶栏 ⋯ 菜单入口 → 状态条从进行中回到空闲（无错误）', async () => {
    await goto('/')
    await waitBoot()
    // 2026-11 改版：「收取新邮件」收进顶栏 ⋯ 菜单（.tb-more），先展开再点
    await openSplit('.tb-more [data-split-toggle]')
    await $('#fetch-btn').click()
    // 状态条进入 running 或直接空闲都接受；关键是不出现错误态
    await browser.waitUntil(
      async () => {
        const bar = await $('#fetch-status')
        const t = (await bar.isExisting()) ? await bar.getText() : ''
        return t.includes('正在收取') || t.includes('没有新邮件') || t.includes('新收')
      },
      { timeout: 60000, timeoutMsg: '状态条未进入已知状态' },
    )
    await shot('收件状态条')
    const text = await $('#fetch-status').getText()
    expect(text).not.toContain('凭据缺失')
    expect(text).not.toContain('超时')
  })
})
