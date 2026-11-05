import { browser, $, $$, expect } from '@wdio/globals'
import { goto, shot, waitBoot, waitText, autoConfirm } from '../support/helpers.mjs'

// 06：联系人（自动采集/搜索/新增/删除/补全接口）与邮件模板（F2–F5）。
// 联系人数据来自 01 首拉时采集的 3 位发件人。
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

    // 删除刚加的（hx-confirm 弹 confirm，已放行）
    await autoConfirm()
    const rows = await $$('.contact-row')
    for (const row of rows) {
      if ((await row.getText()).includes('partner@example.com')) {
        await row.$('.ct-del').click()
        break
      }
    }
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
    // 面板第一张带「添加联系人」按钮的表单是新增模板表单（按钮文案「添加模板」
    // 可能随版本调整，这里按 name 输入框顺序兜底：第一个 name 框 = 新增区）
    const nameInputs = await $$('input[name="name"]')
    await nameInputs[0].setValue('E2E 模板')
    const bodyInputs = await $$('textarea[name="body_text"]')
    await bodyInputs[0].setValue('收到，稍后回复您。')
    await nameInputs[0].$('..').$('..').$('button[type="submit"]').click()
    await browser.waitUntil(
      async () =>
        (await $$('input[name="name"]')).some(
          async (i) => (await i.getValue()) === 'E2E 模板',
        ),
      { timeoutMsg: '模板保存后未出现在列表' },
    )
    await shot('模板-新增成功')
  })

  it('F5b 撰写页模板下拉包含新模板', async () => {
    await goto('/compose')
    await waitBoot()
    const options = await $$('#tpl-select option')
    const texts = []
    for (const o of options) texts.push(await o.getText())
    expect(texts.join('|')).toContain('E2E 模板')
    await shot('模板-撰写页下拉')
  })

  it('F5c 模板删除：列表中消失', async () => {
    await goto('/settings/templates')
    await waitBoot()
    await autoConfirm()
    const before = (await $$('textarea[name="body_text"]')).length
    // 找到包含「E2E 模板」的模板表单里的删除按钮（hx-post=/templates/delete）
    const forms = await $$('form')
    let clicked = false
    for (const f of forms) {
      const nameVal = await f.$('input[name="name"]')
      if (nameVal.isExisting() && (await nameVal.getValue()) === 'E2E 模板') {
        const del = await f.$('button[hx-post*="templates/delete"]')
        await del.click()
        clicked = true
        break
      }
    }
    expect(clicked).toBe(true)
    await browser.waitUntil(
      async () => (await $$('textarea[name="body_text"]')).length === before - 1,
      { timeoutMsg: '模板删除后列表未减少' },
    )
    await shot('模板-删除成功')
  })
})
