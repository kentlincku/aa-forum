import Md from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { ReactNode } from 'react'

// 訊息本文的 Markdown：不吃原始 HTML（react-markdown 預設略過）、javascript: 等連結由預設 urlTransform 清掉。
// 另外把 [task #N] 變成任務連結、@role 變成提及標籤（在 mdast 上轉，不經 HTML 字串）。

type Node = { type: string; value?: string; url?: string; children?: Node[]; data?: Record<string, unknown> }
const PAT = /(\[task #\d+(?: → [\w-]+)?\])|(@[A-Za-z][\w-]*)/g
const HAS = /\[task #\d+|@[A-Za-z]/

function split(text: string): Node[] {
  const out: Node[] = []
  let last = 0
  for (const m of text.matchAll(PAT)) {
    const i = m.index ?? 0
    if (i > last) out.push({ type: 'text', value: text.slice(last, i) })
    if (m[1]) out.push({ type: 'link', url: '#/tasks', data: { hProperties: { className: 'task-ref' } }, children: [{ type: 'text', value: m[1] }] })
    else out.push({ type: 'emphasis', data: { hName: 'span', hProperties: { className: 'mention' } }, children: [{ type: 'text', value: m[2] }] })
    last = i + m[0].length
  }
  if (last < text.length) out.push({ type: 'text', value: text.slice(last) })
  return out
}

function walk(n: Node) {
  if (!n.children || n.type === 'link' || n.type === 'code' || n.type === 'inlineCode') return
  n.children = n.children.flatMap(c => (c.type === 'text' && c.value && HAS.test(c.value) ? split(c.value) : (walk(c), [c])))
}
const refs = () => (tree: Node) => { walk(tree) }

const components = {
  a: ({ href, children, className }: { href?: string; children?: ReactNode; className?: string }) =>
    !href ? <span className={className}>{children}</span>
      : href.startsWith('#') ? <a href={href} className={className}>{children}</a>
      : <a href={href} className={className} target="_blank" rel="noopener noreferrer">{children}</a>,
}

export function Markdown({ text }: { text: string }) {
  return <div className="md"><Md remarkPlugins={[remarkGfm, refs]} components={components}>{text}</Md></div>
}
