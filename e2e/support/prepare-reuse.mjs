#!/usr/bin/env node
// 复用阶段准备脚本（run.sh 在主链路结束后调用）：模拟「重装/换机」状态。
//
// 场景（对应 f932686 首设向导沿用既有数据目录）：
//   corpmail.toml 丢失（配置回到未配置）+ 锚点默认目录的库/附件清空，
//   但把主链路结束时的完整数据（db + wal/shm + attachments）复制一份到
//   run 目录的 reuse-data/，向导在阶段二指向它 → 归属一致则直接沿用。
//
// 为什么复制而不直接用 moved-data（03 迁移的快照）：主链路是单 app 进程
// 贯穿全程，03 的迁移只写了指针 + VACUUM 快照，之后的写入（模板/草稿/
// 发件/04 的删除）全部落在锚点库——锚点库才是「重装前的真实最终状态」。
// 快照（moved-data）只是 mid-run 副本，采纳它测不出数据连续性。
//
// 前置：app 必须已退出（WAL 中可能有未 checkpoint 的事务，运行中复制会
// 拿到撕裂副本）；run.sh 在调用前先 pkill。
//
// 输出：成功时写 artifacts/<run>/reuse-ready.json（含 dataDir），退出码 0；
//       不满足前置（未配置/无库）则不写 marker 退出码 0（阶段二跳过）。

import fs from 'node:fs'
import path from 'node:path'

const runDir = path.resolve(process.argv[2] || '')
if (!runDir || !fs.existsSync(runDir)) {
  console.error('用法：node prepare-reuse.mjs <RUN_DIR>')
  process.exit(2)
}

const anchor = path.join(runDir, 'home', 'Library', 'Application Support', 'com.corpmail.desktop')
const tomlPath = path.join(anchor, 'corpmail.toml')
const anchorDb = path.join(anchor, 'corpmail.db')

// 前置 1：已配置（toml 存在且含账号）——主链路至少跑完向导
let configured = false
if (fs.existsSync(tomlPath)) {
  configured = /^account_prefix\s*=/m.test(fs.readFileSync(tomlPath, 'utf8'))
}
if (!configured) {
  console.log('[reuse-prep] 跳过：主链路未完成配置（无 account_prefix）')
  process.exit(0)
}
// 前置 2：锚点库存在（主链路的实时库）
if (!fs.existsSync(anchorDb)) {
  console.log('[reuse-prep] 跳过：锚点目录没有 corpmail.db')
  process.exit(0)
}

// 1) 数据快照：db（+wal/shm，若未 checkpoint 事务都在里面）复制到 reuse-data/
const reuseData = path.join(runDir, 'reuse-data')
fs.rmSync(reuseData, { recursive: true, force: true })
fs.mkdirSync(reuseData, { recursive: true })
for (const f of ['corpmail.db', 'corpmail.db-wal', 'corpmail.db-shm']) {
  const src = path.join(anchor, f)
  if (fs.existsSync(src)) fs.copyFileSync(src, path.join(reuseData, f))
}
const attachments = path.join(anchor, 'attachments')
if (fs.existsSync(attachments)) {
  fs.cpSync(attachments, path.join(reuseData, 'attachments'), { recursive: true })
}

// 2) 模拟重装：删配置与锚点数据（数据已在 reuse-data/ 副本里；03 迁移残留
//    的 moved-data 快照与过期备份属历史产物，一并清理避免混淆）
fs.rmSync(tomlPath)
for (const f of ['corpmail.db', 'corpmail.db-wal', 'corpmail.db-shm']) {
  fs.rmSync(path.join(anchor, f), { force: true })
}
fs.rmSync(attachments, { recursive: true, force: true })
fs.rmSync(path.join(runDir, 'moved-data'), { recursive: true, force: true })

// 3) marker：阶段二的向导会把它填进「数据保存目录」
const marker = { dataDir: reuseData, preparedAt: new Date().toISOString() }
fs.writeFileSync(path.join(runDir, 'reuse-ready.json'), JSON.stringify(marker, null, 2))

console.log(`[reuse-prep] 已生成重装状态：数据目录=${reuseData}`)
