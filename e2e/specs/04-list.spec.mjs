import { browser, $, $$, expect } from '@wdio/globals'
import {
  goto,
  shot,
  waitBoot,
  waitText,
  openSplit,
  clickByText,
  confirmModalText,
  acceptConfirm,
} from '../support/helpers.mjs'

// 04：列表筛选、排序、翻页、批量操作、高级检索（B5–B16）。
// 数据 = mock 的 3 封 fixture（11-01 欢迎/张伟、11-02 季度报表/李娜/附件、
// 11-03 会议纪要/王强）。批量删除放最后，避免影响前面的行数断言。
//
// 2026-11 UI 改版后的两处结构变化（本文件按新结构写）：
//   1. 筛选（只看未读/只看外部/会话视图）与「收取新邮件」收进顶栏 ⋯ 菜单
//      `.tb-more .split-menu .tb-item`，需先点 [data-split-toggle] 展开
//   2. 批量栏并入顶栏 `#batch-bar`，低频动作（标已读/标未读/星标/取消星标）
//      收进「批量操作⋯」`.ba-more` 菜单；hx-confirm 改走页面内模态
//   3. 2026-11-10：筛选开关自带状态（激活态图标勾选），无「显示全部」项，
//      关闭 = 再点同一项；图标全面 SVG 化（筛选/收取按钮等）
const subjectOf = async (row) => row.$('.mr-subject').getText()

/** 列表顶栏 ⋯ 菜单里点一项（会整页导航） */
async function clickListMenu(text) {
  await openSplit('.tb-more [data-split-toggle]')
  await clickByText('.tb-more .tb-item', text)
}

/** 批量操作⋯ 菜单里点一项（htmx 局部刷新列表） */
async function clickBatchMenu(text) {
  await openSplit('.ba-more [data-split-toggle]')
  await clickByText('.ba-more .split-menu button', text)
}

describe('04 列表筛选与批量操作', () => {
  it('B5 只看未读：⋯ 菜单开关后所有行都带 unread 类，再点同一项切回恢复全量', async () => {
    await goto('/')
    await waitBoot()
    const before = (await $$('.mail-row')).length
    expect(before).toBeGreaterThanOrEqual(2) // 02 已读过季度报表，欢迎/会议仍未读

    await clickListMenu('只看未读')
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('unread=1'),
      { timeoutMsg: '未读开关未生效（⋯ 菜单里的入口）' },
    )
    await waitBoot()
    const rows = await $$('.mail-row')
    expect(rows.length).toBeGreaterThanOrEqual(1)
    for (const r of rows) {
      expect(await r.getAttribute('class')).toContain('unread')
    }
    await shot('列表-只看未读')

    // 2026-11-10：开关自带状态（激活态图标 ✓），不再有独立的「显示全部」项；
    // 再点同一项即关闭（unread_href 在激活态指向无参路径）
    await clickListMenu('只看未读')
    await browser.waitUntil(
      async () => !(await browser.getUrl()).includes('unread=1'),
      { timeoutMsg: '再点「只看未读」未取消过滤' },
    )
    await waitBoot()
    await browser.waitUntil(
      async () => (await $$('.mail-row')).length === before,
      { timeoutMsg: '切回全部后行数未恢复' },
    )
    await shot('列表-取消未读过滤')
  })

  it('B6 只看外部：⋯ 菜单入口，mock 发件人全是内部域 → 空态文案', async () => {
    await clickListMenu('只看外部')
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('external=1'),
      { timeoutMsg: '外部过滤未生效' },
    )
    await waitBoot()
    expect((await $$('.mail-row')).length).toBe(0)
    await waitText('.main', '没有外部邮件')
    await shot('列表-只看外部（空态）')

    // 恢复全量：同 B5，再点同一项关闭（无「显示全部」入口）
    await clickListMenu('只看外部')
    await browser.waitUntil(
      async () => !(await browser.getUrl()).includes('external=1'),
      { timeoutMsg: '再点「只看外部」未取消过滤' },
    )
    await waitBoot()
    await browser.waitUntil(async () => (await $$('.mail-row')).length >= 3, {
      timeoutMsg: '外部过滤取消后未恢复全量',
    })
  })

  it('B7 排序：date_asc 首行为最早邮件，subject_asc 首行为「会议纪要」', async () => {
    await goto('/?sort=date_asc')
    await waitBoot()
    expect(await subjectOf(await $$('.mail-row')[0])).toContain('欢迎使用')
    await shot('列表-最早优先')

    await goto('/?sort=subject_asc')
    await waitBoot()
    expect(await subjectOf(await $$('.mail-row')[0])).toContain('会议纪要')
    await shot('列表-按主题排序')
  })

  it('B8 每页条数 per=1：单行 + 首页/末页翻页', async () => {
    await goto('/?per=1')
    await waitBoot()
    expect((await $$('.mail-row')).length).toBe(1)
    const first = await subjectOf(await $$('.mail-row')[0])
    // 第 1 页不渲染「首页/上一页」，但渲染「下一页」与「末页」
    const firstPageLinks = await browser.execute(() =>
      [...document.querySelectorAll('.pager a.pg')].map((a) => a.textContent.trim()),
    )
    expect(firstPageLinks.some((t) => t.includes('首页'))).toBe(false)
    expect(firstPageLinks.some((t) => t.includes('上一页'))).toBe(false)
    expect(firstPageLinks.some((t) => t.includes('下一页'))).toBe(true)
    expect(firstPageLinks.some((t) => t.includes('末页'))).toBe(true)

    await $('a.pg').click() // 下一页
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('page=2'),
      { timeoutMsg: '翻页未发生' },
    )
    await waitBoot()
    expect(await subjectOf(await $$('.mail-row')[0])).not.toBe(first)
    // 中间页：首页/上一页/下一页/末页四件套齐全
    const midLinks = await browser.execute(() =>
      [...document.querySelectorAll('.pager a.pg')].map((a) => a.textContent.trim()),
    )
    for (const label of ['首页', '上一页', '下一页', '末页']) {
      expect(midLinks.some((t) => t.includes(label))).toBe(true)
    }
    await shot('列表-每页1封翻页')

    // 末页 → 最后一页（3 封 → page=3），只剩首页/上一页
    await clickByText('.pager a.pg', '末页')
    await browser.waitUntil(
      async () => /page=3\b/.test(await browser.getUrl()),
      { timeoutMsg: '末页未跳到最后一页' },
    )
    await waitBoot()
    const lastLinks = await browser.execute(() =>
      [...document.querySelectorAll('.pager a.pg')].map((a) => a.textContent.trim()),
    )
    expect(lastLinks.some((t) => t.includes('下一页'))).toBe(false)
    expect(lastLinks.some((t) => t.includes('末页'))).toBe(false)
    await shot('列表-末页')

    // 首页 → 回到 page=1
    await clickByText('.pager a.pg', '首页')
    await browser.waitUntil(
      async () => /page=1\b/.test(await browser.getUrl()),
      { timeoutMsg: '首页未跳回第 1 页' },
    )
    await waitBoot()

    // per 持久化到 settings：恢复默认，避免影响后续 spec 的行数断言
    await goto('/?per=10')
    await waitBoot()
  })

  it('B9 文件夹切换与侧栏计数：sent/draft 空态，收件箱有未读角标', async () => {
    await goto('/?folder=sent')
    await waitBoot()
    await waitText('h1', '发件箱')
    expect((await $$('.mail-row')).length).toBe(0)
    await shot('列表-发件箱空态')

    await goto('/?folder=draft')
    await waitBoot()
    await waitText('h1', '草稿箱')
    await shot('列表-草稿箱空态')

    await goto('/')
    await waitBoot()
    await browser.waitUntil(
      async () => (await $('.side .unread-chip').isExisting()) ||
        (await $('#fetch-status').getText()).includes('0 封新'),
      { timeoutMsg: '侧栏未读角标未出现（收件箱应有未读）' },
    )
    await shot('侧栏-未读角标')
  })

  it('B10 批量星标/取消：全选 → 批量操作⋯ 菜单 star/unstar', async () => {
    await goto('/')
    await waitBoot()
    await $('#batch-all').click()
    await clickBatchMenu('星标')
    await browser.waitUntil(
      async () => (await $$('.mail-row.starred')).length === (await $$('.mail-row')).length,
      { timeoutMsg: '批量星标后未全部置顶标金' },
    )
    await shot('列表-批量星标')

    await $('#batch-all').click()
    await clickBatchMenu('取消星标')
    await browser.waitUntil(
      async () => (await $$('.mail-row.starred')).length === 0,
      { timeoutMsg: '批量取消星标未生效' },
    )
    await shot('列表-取消星标')
  })

  it('B15 批量标已读/标未读 + 清空选择（批量操作⋯ 菜单）', async () => {
    await goto('/')
    await waitBoot()
    await $('#batch-all').click()
    await clickBatchMenu('标已读')
    await browser.waitUntil(
      async () => (await $$('.mail-row.unread')).length === 0,
      { timeoutMsg: '批量标已读后仍有未读行' },
    )
    await shot('列表-批量标已读')

    await $('#batch-all').click()
    await clickBatchMenu('标未读')
    await browser.waitUntil(
      async () => (await $$('.mail-row.unread')).length === (await $$('.mail-row')).length,
      { timeoutMsg: '批量标未读未生效' },
    )
    await shot('列表-批量标未读')

    // 清空选择：先全选让计数有值，再点「清空选择」→ 计数归零、全选盒取消勾选
    await $('#batch-all').click()
    const before = await browser.execute(
      () => document.getElementById('ba-count')?.textContent?.trim() ?? '',
    )
    expect(before.length).toBeGreaterThan(0)
    await $('#ba-clear').click()
    const cleared = await browser.execute(() => ({
      count: (document.getElementById('ba-count')?.textContent ?? '').trim(),
      all: document.getElementById('batch-all')?.checked,
    }))
    expect(cleared.count).toBe('')
    expect(cleared.all).toBe(false)
    await shot('列表-清空选择')
  })

  it('B16 高级检索面板：默认收起 → 高级展开 → 应用筛选后面板保持展开', async () => {
    await goto('/')
    await waitBoot()
    const hiddenAtStart = await browser.execute(
      () => document.getElementById('adv-panel').hidden,
    )
    expect(hiddenAtStart).toBe(true) // 无激活条件时默认收起

    await $('#adv-toggle').click()
    await browser.waitUntil(
      async () =>
        !(await browser.execute(() => document.getElementById('adv-panel').hidden)),
      { timeoutMsg: '点「高级」后面板未展开' },
    )
    await shot('检索-高级面板展开')

    // 勾「只看有附件」→ 应用筛选（GET 表单，保留 folder）
    await browser.execute(() => {
      const cb = document.querySelector('#adv-panel input[name="att"]')
      cb.checked = true
    })
    await $('#adv-panel button[type="submit"]').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('att=1'),
      { timeoutMsg: '应用筛选未提交 att=1' },
    )
    await waitBoot()
    const rows = await $$('.mail-row')
    expect(rows.length).toBe(1)
    expect(await subjectOf(rows[0])).toContain('季度报表')
    // 2026-11-09 改版：应用筛选后面板保持展开（否则用户看不到/改不了当前
    // 条件）；入口按钮带 on 状态表示有激活筛选
    const keptOpen = await browser.execute(() => ({
      panelOpen: !document.getElementById('adv-panel').hidden,
      toggleOn: document.getElementById('adv-toggle')?.classList.contains('on'),
    }))
    expect(keptOpen.panelOpen).toBe(true)
    expect(keptOpen.toggleOn).toBe(true)
    await shot('检索-应用筛选后面板保持展开')

    // 面板已展开，直接清除筛选 → 恢复全量（旧版需要先再点—次开关，
    // 新行为下再点会收起反而点不到清除按钮）
    await $('#adv-clear').click()
    await browser.waitUntil(async () => (await $$('.mail-row')).length >= 3, {
      timeoutMsg: '清除筛选后未恢复全量',
    })
    await waitBoot()
  })

  it('B12 高级检索·发件人：from=张伟 命中欢迎邮件', async () => {
    await goto('/?from=%E5%BC%A0%E4%BC%9F') // 张伟
    await waitBoot()
    const rows = await $$('.mail-row')
    expect(rows.length).toBe(1)
    expect(await subjectOf(rows[0])).toContain('欢迎使用')
    await shot('检索-按发件人')
  })

  it('B13 高级检索·仅看附件：att=1 命中季度报表', async () => {
    await goto('/?att=1')
    await waitBoot()
    const rows = await $$('.mail-row')
    expect(rows.length).toBe(1)
    expect(await subjectOf(rows[0])).toContain('季度报表')
    await shot('检索-仅看有附件')
  })

  it('B14 高级检索·日期区间：11-02 当天只命中季度报表', async () => {
    await goto('/?df=2026-11-02&dt=2026-11-02')
    await waitBoot()
    const rows = await $$('.mail-row')
    expect(rows.length).toBe(1)
    expect(await subjectOf(rows[0])).toContain('季度报表')
    await shot('检索-日期区间')
  })

  it('B11 批量删除：勾选 1 行 → 确认模态 → 行数 -1（放最后）', async () => {
    await goto('/')
    await waitBoot()
    const before = (await $$('.mail-row')).length
    const rows = await $$('.mail-row')
    await rows.at(-1).$('.mbx-item').click() // 删最后一行：保住 05 依赖的「会议纪要」
    await $('button[data-batch="delete"]').click()
    // 2026-11：hx-confirm 改页面内模态（window.confirm 在 WKWebView 恒 false）
    const question = await confirmModalText()
    expect(question).toContain('删除')
    await acceptConfirm()
    await browser.waitUntil(
      async () => (await $$('.mail-row')).length === before - 1,
      { timeout: 20000, timeoutMsg: '批量删除后行数未减少' },
    )
    await shot('列表-批量删除')
  })
})
