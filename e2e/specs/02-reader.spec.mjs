import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText, modalOpen } from '../support/helpers.mjs'

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
    await browser.waitUntil(async () => (await modalOpen()) === true, {
      timeout: 10000,
      timeoutMsg: '预览模态未打开',
    })
    await shot('读信-附件预览')
    await $('.pm-close').click()
    await browser.waitUntil(async () => (await modalOpen()) === false, {
      timeout: 5000,
      timeoutMsg: '预览模态未关闭',
    })
  })

  it('中文关键词检索命中单封邮件', async () => {
    await goto('/')
    const input = await $('form.search input')
    await input.setValue('季度')
    // 插件对合成 Enter 只模拟换行/编辑副作用，不模拟「文本框回车隐式提交表单」
    // （key_input.rs line_break_js），browser.keys('Enter') 永远不导航。
    // 用 requestSubmit 触发真实 GET 导航。
    await browser.execute(() => document.querySelector('form.search').requestSubmit())
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
    // 2026-11-09 改版：q 非空时搜索框右侧是「清除」链接（.adv-toggle 复用自
    // 高级检索入口位），原 search-clear × 已随图标 SVG 化移除
    await $('a.adv-toggle').click()
    await waitBoot()
    await browser.waitUntil(async () => (await $$('.mail-row')).length >= 3, {
      timeout: 15000,
      timeoutMsg: '清空检索后未恢复全量列表',
    })
    await shot('检索-清空恢复')
  })

  // 2026-11 UI 改版：动作区整行移入顶栏、标题并入正文卡片、预览弹窗去掉
  // 工具栏行（Office/PDF 另存入口收敛到详情页附件列表）
  it('详情页结构：动作在顶栏、标题在卡片内、预览无工具栏', async () => {
    const rows = await $$('.mail-row')
    for (const row of rows) {
      if ((await row.getText()).includes('季度报表')) {
        await row.$('.mr-link').click()
        break
      }
    }
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('/emails/'),
      { timeoutMsg: '未进入详情页' },
    )
    await waitBoot()

    const s = await browser.execute(() => {
      const tb = document.querySelector('.tb-actions')
      const card = document.querySelector('.mail-detail')
      return {
        hasReply: !!tb?.querySelector('a[href*="reply_to"]'),
        hasForward: !!tb?.querySelector('a[href*="forward_from"]'),
        hasDelete: !!tb?.querySelector('button[hx-post*="/delete"]'),
        splitCount: tb?.querySelectorAll('[data-split-toggle]').length ?? 0,
        titleInCard: !!card?.querySelector('.md-detail-title'),
        toolbarGone: !document.getElementById('pm-toolbar'),
      }
    })
    expect(s.hasReply).toBe(true)
    expect(s.hasForward).toBe(true)
    expect(s.hasDelete).toBe(true)
    expect(s.splitCount).toBeGreaterThanOrEqual(2) // 回复▾ + ⋯
    expect(s.titleInCard).toBe(true)
    expect(s.toolbarGone).toBe(true)
    await shot('读信-顶栏动作区')
  })
})
