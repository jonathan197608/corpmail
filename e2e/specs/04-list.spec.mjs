import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText, autoConfirm } from '../support/helpers.mjs'

// 04：列表筛选、排序、翻页、批量操作、高级检索（B5–B14）。
// 数据 = mock 的 3 封 fixture（11-01 欢迎/张伟、11-02 季度报表/李娜/附件、
// 11-03 会议纪要/王强）。批量删除放最后，避免影响前面的行数断言。
const subjectOf = async (row) => row.$('.mr-subject').getText()

describe('04 列表筛选与批量操作', () => {
  it('B5 只看未读：开关后所有行都带 unread 类，切回恢复全量', async () => {
    await goto('/')
    await waitBoot()
    const before = (await $$('.mail-row')).length
    expect(before).toBeGreaterThanOrEqual(2) // 02 已读过季度报表，欢迎/会议仍未读

    await $('a.unread-toggle:not(.ext-toggle)').click()
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('unread=1'),
      { timeoutMsg: '未读开关未生效' },
    )
    await waitBoot()
    const rows = await $$('.mail-row')
    expect(rows.length).toBeGreaterThanOrEqual(1)
    for (const r of rows) {
      expect(await r.getAttribute('class')).toContain('unread')
    }
    await shot('列表-只看未读')

    await $('a.unread-toggle:not(.ext-toggle)').click()
    await waitBoot()
    await browser.waitUntil(
      async () => (await $$('.mail-row')).length === before,
      { timeoutMsg: '切回全部后行数未恢复' },
    )
    await shot('列表-取消未读过滤')
  })

  it('B6 只看外部：mock 发件人全是内部域 → 空态文案', async () => {
    await goto('/?external=1')
    await waitBoot()
    expect((await $$('.mail-row')).length).toBe(0)
    await waitText('.main', '没有外部邮件')
    await shot('列表-只看外部（空态）')
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

  it('B8 每页条数 per=1：单行 + 翻页到第 2 页显示另一封', async () => {
    await goto('/?per=1')
    await waitBoot()
    expect((await $$('.mail-row')).length).toBe(1)
    const first = await subjectOf(await $$('.mail-row')[0])

    await $('a.pg').click() // 下一页
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('page=2'),
      { timeoutMsg: '翻页未发生' },
    )
    await waitBoot()
    expect(await subjectOf(await $$('.mail-row')[0])).not.toBe(first)
    await shot('列表-每页1封翻页')

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

  it('B10 批量星标/取消：全选 → star → 行带 starred；unstar 恢复', async () => {
    await goto('/')
    await waitBoot()
    await $('#batch-all').click()
    await $('button[data-batch="star"]').click()
    await browser.waitUntil(
      async () => (await $$('.mail-row.starred')).length === (await $$('.mail-row')).length,
      { timeoutMsg: '批量星标后未全部置顶标金' },
    )
    await shot('列表-批量星标')

    await $('#batch-all').click()
    await $('button[data-batch="unstar"]').click()
    await browser.waitUntil(
      async () => (await $$('.mail-row.starred')).length === 0,
      { timeoutMsg: '批量取消星标未生效' },
    )
    await shot('列表-取消星标')
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

  it('B11 批量删除：勾选 1 行删除后行数 -1（放最后，减少后续用例数据）', async () => {
    await goto('/')
    await waitBoot()
    await autoConfirm()
    const before = (await $$('.mail-row')).length
    const rows = await $$('.mail-row')
    await rows.at(-1).$('.mbx-item').click() // 删最后一行：保住 05 依赖的「会议纪要」
    await $('button[data-batch="delete"]').click()
    await browser.waitUntil(
      async () => (await $$('.mail-row')).length === before - 1,
      { timeout: 20000, timeoutMsg: '批量删除后行数未减少' },
    )
    await shot('列表-批量删除')
  })
})
