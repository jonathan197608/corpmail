import { browser, $, $$, expect } from '@wdio/globals'
import { shot, waitBoot } from '../support/helpers.mjs'

// 00：未配置状态。run 目录是全新的 HOME，所以这里必然落在欢迎页。
describe('00 启动与欢迎页', () => {
  it('首帧渲染欢迎页，启动遮罩已移除', async () => {
    await waitBoot()
    await expect(browser).toHaveTitle('CorpMail')
    await expect($('h1')).toHaveText(expect.stringContaining('欢迎使用 CorpMail'))
    await shot('欢迎页')
  })

  it('三条产品说明与「开始配置」入口可见', async () => {
    await expect($$('.feature-row')).toBeElementsArrayOfSize(3)
    await expect($('a.btn.primary.big')).toHaveText(expect.stringContaining('开始配置'))
    await shot('欢迎页-功能说明')
  })

  it('侧栏在未配置时不显示收件状态（无假错误）', async () => {
    // 回归：启动 2s 首收曾在向导之前以空账号跑一轮，把「凭据缺失」写进状态条
    const status = await $('#fetch-status')
    const text = (await status.isExisting()) ? await status.getText() : ''
    expect(text).not.toContain('凭据缺失')
  })

  it('图标 SVG 化：功能卡线性图标在场，正文不出现 emoji/文本符号', async () => {
    // 2026-11-10 全站图标改内联 SVG（跨平台线重/基线一致）后的回归守卫：
    // 欢迎页 3 张特性卡图标必须以内联 <svg class="ic"> 渲染，且页面文字
    // 不得再出现 emoji 或 ✓✗✕● 一类文本符号（与单测禁用清单同源）
    const iconCount = await browser.execute(
      () => document.querySelectorAll('svg.ic').length,
    )
    expect(iconCount).toBeGreaterThanOrEqual(3)
    const text = await browser.execute(() => document.body.innerText)
    expect(text).not.toMatch(
      /[✓✗✕●×⭐📎🗑📥📤📝👥⚙🔍🔎🔄⌨🖨💾📄📨📁🔒🗄⚠⏰⏳✏🌐🎛💬]/,
    )
    await shot('欢迎页-SVG图标')
  })
})
