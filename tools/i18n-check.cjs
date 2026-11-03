#!/usr/bin/env node
// i18n 檢查：所有 tr('...') 用到的字串在 zh-TW 對照表都有；對照表沒有多餘的 key。
// 用法：node tools/i18n-check.cjs [--add file.json]（--add 把 JSON 物件併入對照表）
const fs = require('fs')
const path = require('path')
const ts = require('../web/node_modules/typescript')
const root = path.join(__dirname, '../web/src')
const i18n = path.join(root, 'i18n.ts')
let text = fs.readFileSync(i18n, 'utf8')
const start = text.indexOf('const ZH: Record<string, string> = ') + 'const ZH: Record<string, string> = '.length
const end = text.indexOf('\n}\n', start) + 2
let zh = JSON.parse(text.slice(start, end))
const ai = process.argv.indexOf('--add')
if (ai > 0) {
  Object.assign(zh, JSON.parse(fs.readFileSync(process.argv[ai + 1], 'utf8')))
  text = text.slice(0, start) + JSON.stringify(zh, null, 1) + text.slice(end)
  fs.writeFileSync(i18n, text)
}
const used = new Set()
const walk = d => fs.readdirSync(d).forEach(f => {
  const p = path.join(d, f)
  if (fs.statSync(p).isDirectory()) return walk(p)
  if (!p.endsWith('.tsx') && !p.endsWith('.ts')) return
  const sf = ts.createSourceFile(p, fs.readFileSync(p, 'utf8'), ts.ScriptTarget.Latest, true, p.endsWith('x') ? ts.ScriptKind.TSX : ts.ScriptKind.TS)
  const v = n => {
    if (ts.isCallExpression(n) && n.expression.getText(sf) === 'tr' && n.arguments[0] && ts.isStringLiteralLike(n.arguments[0])) used.add(n.arguments[0].text)
    ts.forEachChild(n, v)
  }
  v(sf)
})
walk(root)
// 動態 key：nav（伺服器給）、任務狀態、回合結果、agent 狀態（tr(變數)）
for (const n of ['open', 'claimed', 'blocked', 'done', 'cancelled', 'ok', 'running', 'failed', 'incomplete', 'start_failed', 'working', 'idle', 'offline', 'starting', 'not started', 'could not start', 'turn failed', 'Premium requests', 'Chat', 'Completions', '5 hours', 'Weekly', 'Rooms', 'Search', 'Tasks', 'Trash', 'Files', 'Skills', 'Dashboard', 'Agents', 'active', 'archived', 'deleted', '{by} froze the room', '{by} unfroze the room', '{by} turned auto-approve on', '{by} turned auto-approve off']) used.add(n)
const missing = [...used].filter(k => !(k in zh)).sort()
const unused = Object.keys(zh).filter(k => !used.has(k)).sort()
console.log(JSON.stringify({ used: used.size, zh: Object.keys(zh).length, missing, unused }, null, 1))
process.exit(missing.length ? 1 : 0)
