import { browser, $, $$, expect } from '@wdio/globals'
import {
  goto,
  shot,
  waitBoot,
  waitText,
  confirmModalText,
  acceptConfirm,
} from '../support/helpers.mjs'

// 06：联系人（自动采集/搜索/新增/删除/补全接口）与邮件模板（F2–F5）。
// 联系人数据来自 01 首拉时采集的 3 位发件人。
// 模板名「E2E 模板」专属于本 spec（05 D1 用「E2E 撰写页预置模板」）：
// templates 表无唯一约束，同名并存会让存在性断言假绿。
const TPL_NAME = 'E2E 模板'

/** 精确按名称统计模板条目数（.tpl-title 是列表里的模板名） */
async function countTemplates(name) {
  const titles = await $$('.tpl-title')
  let n = 0
  for (const t of titles) {
    if ((await t.getText()).trim() === name) n += 1
  }
  return n
}

describe('06 联系人与模板', () => {
  it('F2 联系人自动采集：3 位发件人在列且带姓名', async () => {
    await goto('/contacts')
    await waitBoot()
    await waitText('h1', '联系人')
    expect((await $$('.contact-row')).length).toBeGreaterThanOrEqual(3)
    const text = await $('body').getText()
    expect(text).toContain('zhangwei@corpmail.test')
    expect(text).toContain('lina@corpmail.test')
    expect(text).toContain('wangqiang@corpmail.test')
    await shot('联系人-自动采集')
  })

  it('F3a 联系人搜索：按姓名过滤到 1 行', async () => {
    await goto('/contacts')
    await waitBoot()
    await $('#contacts-search input[name="q"]').setValue('李娜')
    // 同 02：合成 Enter 不触发隐式表单提交，用 requestSubmit
    await browser.execute(() => document.getElementById('contacts-search').requestSubmit())
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('q='),
      { timeoutMsg: '联系人搜索未提交' },
    )
    await waitBoot()
    expect((await $$('.contact-row')).length).toBe(1)
    await shot('联系人-搜索过滤')
  })

  it('F3b 新增联系人 → 列表出现；删除 → 消失', async () => {
    await goto('/contacts')
    await waitBoot()
    await $('form.ct-form input[name="addr"]').setValue('partner@example.com')
    await $('form.ct-form input[name="name"]').setValue('外部伙伴')
    await $('form.ct-form button[type="submit"]').click()
    await browser.waitUntil(
      async () => (await $('body').getText()).includes('partner@example.com'),
      { timeoutMsg: '新增联系人未出现在列表' },
    )
    await shot('联系人-新增成功')

    // 删除刚加的（hx-confirm 走页面内模态，window.confirm 在 WKWebView 恒 false）
    const rows = await $$('.contact-row')
    for (const row of rows) {
      if ((await row.getText()).includes('partner@example.com')) {
        await row.$('.ct-del').click()
        break
      }
    }
    const question = await confirmModalText()
    expect(question).toContain('partner@example.com')
    await acceptConfirm()
    await browser.waitUntil(
      async () => !(await $('body').getText()).includes('partner@example.com'),
      { timeoutMsg: '删除联系人后行未消失' },
    )
    await shot('联系人-删除成功')
  })

  it('F4 收件人补全接口：suggest?q=li 返回 lina 地址（页面 fetch 断言）', async () => {
    const result = await browser.execute(async () => {
      const r = await fetch('/contacts/suggest?q=li')
      return await r.json()
    })
    expect(JSON.stringify(result)).toContain('lina@corpmail.test')
    await shot('联系人-补全接口')
  })

  it('F5a 模板新增：保存后列表出现', async () => {
    await goto('/settings/templates')
    await waitBoot()
    const before = await countTemplates(TPL_NAME)
    // 新增表单是 pane 里的 .tpl-form（列表项的编辑表单在 details 内，不带
    // .tpl-form 类），按类定位避免与前序 spec 留下的模板混淆
    await $('.tpl-form input[name="name"]').setValue(TPL_NAME)
    await $('.tpl-form textarea[name="body_text"]').setValue('收到，稍后回复您。')
    await $('.tpl-form button[type="submit"]').click()
    // 计数 +1 才算真的新增（只判「存在同名」会被前序数据蒙混过关）
    await browser.waitUntil(
      async () => (await countTemplates(TPL_NAME)) === before + 1,
      { timeoutMsg: '模板保存后列表未新增该模板' },
    )
    await shot('模板-新增成功')
  })

  it('F5b 撰写页模板下拉包含新模板', async () => {
    await goto('/compose')
    await waitBoot()
    // option 文本用 execute 读：WKWebView 下 option.getText 不可靠（同 05 D1）
    const texts = await browser.execute(() =>
      [...document.querySelectorAll('#tpl-select option')].map((o) => o.textContent.trim()),
    )
    expect(texts).toContain(TPL_NAME) // 全等，避免子串蒙混
    await shot('模板-撰写页下拉')
  })

  it('F5c 模板删除：确认模态后列表中消失', async () => {
    await goto('/settings/templates')
    await waitBoot()
    const before = await countTemplates(TPL_NAME)
    expect(before).toBeGreaterThanOrEqual(1)
    // 删除按钮在 .tpl-head 里（与 details > form 平级，不在编辑表单内），
    // 必须从 .tpl-item 作用域找，不能从 form 里找
    const items = await $$('.tpl-item')
    let clicked = false
    for (const it of items) {
      const title = await it.$('.tpl-title')
      if ((await title.getText()).trim() === TPL_NAME) {
        await it.$('button[hx-post*="templates/delete"]').click()
        clicked = true
        break
      }
    }
    expect(clicked).toBe(true)
    const question = await confirmModalText()
    expect(question).toContain(TPL_NAME)
    await acceptConfirm()
    await browser.waitUntil(
      async () => (await countTemplates(TPL_NAME)) === before - 1,
      { timeoutMsg: '模板删除后列表未减少' },
    )
    await shot('模板-删除成功')
  })

  it('F6 联系人分页与排序：sort 下拉回显 + per=1 翻页', async () => {
    await goto('/contacts')
    await waitBoot()
    // 排序下拉默认「最近联系」；选项文案与邮件列表对齐
    const opts = await browser.execute(() =>
      [...document.querySelectorAll('.per-form select[name="sort"] option')].map(
        (o) => o.textContent.trim(),
      ),
    )
    expect(opts).toEqual(['最近联系', '联系最多', '姓名'])
    expect(
      await browser.execute(
        () => document.querySelector('.per-form select[name="sort"]').value,
      ),
    ).toBe('recent')

    const all = (await $$('.contact-row')).length
    expect(all).toBeGreaterThanOrEqual(3)

    await goto('/contacts?per=1')
    await waitBoot()
    expect((await $$('.contact-row')).length).toBe(1)
    // 用地址标识行，不用姓名：采集不到 display name 的联系人统一显示「（未命名）」
    const firstAddr = (await (await $$('.contact-row'))[0].$('.ct-addr').getText()).trim()
    expect(firstAddr).toContain('@')
    await shot('联系人-每页1人')

    await $('.pager a.pg').click() // 第 1 页只有「下一页」
    await browser.waitUntil(
      async () => (await browser.getUrl()).includes('page=2'),
      { timeoutMsg: '联系人翻页未发生' },
    )
    await waitBoot()
    expect((await $$('.contact-row')).length).toBe(1)
    const secondAddr = (await (await $$('.contact-row'))[0].$('.ct-addr').getText()).trim()
    expect(secondAddr).not.toBe(firstAddr)
    await shot('联系人-第2页')

    // per 与邮件列表共用 settings 的 PAGE_SIZE：恢复默认，避免污染后续行数断言
    await goto('/contacts?per=10')
    await waitBoot()
  })

  it('F7 模板富文本：升级 → HTML 源码写入 → 保存后列表出现，再删除', async () => {
    const RICH = 'E2E 富文本模板'
    await goto('/settings/templates')
    await waitBoot()
    const before = await countTemplates(RICH)

    await $('.tpl-form input[name="name"]').setValue(RICH)
    await $('.tpl-form [data-tpl-upgrade]').click()
    // 单向升级：纯文本区隐藏、html 区显示、隐藏域 body_mode=html
    await browser.waitUntil(
      async () =>
        browser.execute(() => {
          const f = document.querySelector('.tpl-form [data-tpl-field]')
          return (
            f &&
            !f.querySelector('[data-tpl-html-field]').hidden &&
            f.querySelector('[data-tpl-mode]').value === 'html'
          )
        }),
      { timeoutMsg: '切换富文本后 html 编辑区未显示' },
    )
    // 直接写 HTML 源码 textarea：不依赖 wangEditor 是否初始化成功
    // （初始化失败时 app.js 自动降级到源码 textarea，两条路径都走同一字段）
    await browser.execute(() => {
      document.querySelector('.tpl-form textarea[name="body_html"]').value =
        '<p>富文本模板正文 <b>加粗</b></p>'
    })
    await $('.tpl-form button[type="submit"]').click()
    await waitText('#settings-pane', '✓ 已保存')
    await browser.waitUntil(
      async () => (await countTemplates(RICH)) === before + 1,
      { timeoutMsg: '富文本模板保存后列表未新增' },
    )
    await shot('模板-富文本新增成功')

    // 撰写页下拉应包含它（html 模板也在列表里）
    await goto('/compose')
    await waitBoot()
    const texts = await browser.execute(() =>
      [...document.querySelectorAll('#tpl-select option')].map((o) =>
        o.textContent.trim(),
      ),
    )
    expect(texts).toContain(RICH)

    // 清理：删除该模板（顺带再走一次确认模态）
    await goto('/settings/templates')
    await waitBoot()
    const items = await $$('.tpl-item')
    for (const it of items) {
      if ((await (await it.$('.tpl-title')).getText()).trim() === RICH) {
        await it.$('button[hx-post*="templates/delete"]').click()
        break
      }
    }
    await confirmModalText()
    await acceptConfirm()
    await browser.waitUntil(
      async () => (await countTemplates(RICH)) === before,
      { timeoutMsg: '富文本模板删除后列表未恢复' },
    )
    await shot('模板-富文本已清理')
  })
})
