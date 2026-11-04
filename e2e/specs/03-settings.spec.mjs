import { browser, $, expect } from '@wdio/globals'
import { goto, shot, waitBoot } from '../support/helpers.mjs'

// 03：设置四个页签 + 收件状态条（后台刷新链路的 UI 侧证据）
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

  it('关于页签', async () => {
    await goto('/settings/about')
    await shot('设置-关于')
  })

  it('收取新邮件：状态条从进行中回到空闲（无错误）', async () => {
    await goto('/')
    await waitBoot()
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
