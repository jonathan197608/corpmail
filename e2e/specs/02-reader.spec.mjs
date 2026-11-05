import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText } from '../support/helpers.mjs'

// 02：读信、附件、中文检索、已读切换。
// 数据来自 mock 的 3 封 fixture（含一封 HTML + CSV 附件）。
describe('02 读信与检索', () => {
  it('打开含附件的邮件：正文与附件清单渲染', async () => {
    await waitBoot()
    const rows = await $$('.mail-row')
    let opened = false
    for (const row of rows) {
      if ((await row.getText()).includes('季度报表')) {
        await row.$('.mr-link').click()
        opened = true
        break
      }
    }
    expect(opened).toBe(true)

    await browser.waitUntil(
      // 默认列表为单封模式（/emails/{id}）；mode=thread 才是会话视图（/threads/）
      async () => /\/(threads|emails)\//.test(await browser.getUrl()),
      { timeout: 15000, timeoutMsg: '未进入读信页' },
    )
    await waitBoot()
    await shot('读信-正文与附件')

    expect(await $('body').getText()).toContain('季度报表')
    // 附件清单：email.html 的 .att-list / .att-row / .att-name
    await waitText('.att-list', 'report.csv')
    expect((await $$('.att-row')).length).toBe(1)
  })

  it('点击附件名打开预览模态（CSV 走文本预览）', async () => {
    await $('.att-name.clickable').click()
    await browser.waitUntil(
      async () => (await $('#preview-modal')).isDisplayed(),
      { timeout: 10000, timeoutMsg: '预览模态未打开' },
    )
    await shot('读信-附件预览')
    await $('.pm-close').click()
    await browser.waitUntil(async () => !(await $('#preview-modal')).isDisplayed(), {
      timeout: 5000,
      timeoutMsg: '预览模态未关闭',
    })
  })

  it('中文关键词检索命中单封邮件', async () => {
    await goto('/')
    const input = await $('form.search input')
    await input.setValue('季度')
    await browser.keys('Enter')
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('q='),
      { timeout: 15000, timeoutMsg: '搜索未提交' },
    )
    await waitBoot()
    await waitText('.mail-list', '季度报表')
    expect((await $$('.mail-row')).length).toBe(1)
    await shot('检索-中文关键词命中')
  })

  it('清空检索恢复全量列表', async () => {
    await $('a.search-clear').click()
    await waitBoot()
    await browser.waitUntil(async () => (await $$('.mail-row')).length >= 3, {
      timeout: 15000,
      timeoutMsg: '清空检索后未恢复全量列表',
    })
    await shot('检索-清空恢复')
  })
})
