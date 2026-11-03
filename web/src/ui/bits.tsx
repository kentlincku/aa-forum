import { useEffect, useRef, useState, type ReactNode } from 'react'
import { t as tr } from '../i18n'

/** 頭像：名字首字＋依名字雜湊的顏色（不上傳）。 */
export function Avatar({ name, kind }: { name: string; kind?: string }) {
  let h = 0
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) % 360
  return <span className={`avatar ${kind ?? ''}`} style={{ background: `hsl(${h} 55% 45%)` }} aria-hidden>{name.slice(0, 1).toUpperCase()}</span>
}

/** 「⋯」選單：低頻與危險動作收在這裡。點外面或按 Esc 關閉。 */
export function MoreMenu({ label, children }: { label?: string; children: ReactNode }) {
  const [open, setOpen] = useState(false)
  const box = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!open) return
    const off = (e: MouseEvent) => { if (!box.current?.contains(e.target as Node)) setOpen(false) }
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false) }
    document.addEventListener('mousedown', off); document.addEventListener('keydown', esc)
    return () => { document.removeEventListener('mousedown', off); document.removeEventListener('keydown', esc) }
  }, [open])
  return (
    <div className="more" ref={box}>
      <button className="ghost icon" aria-label={label ?? 'more'} aria-expanded={open} title={tr('More')} onClick={() => setOpen(v => !v)}>⋯</button>
      {open && <div className="more-list" role="menu" onClick={() => setOpen(false)}>{children}</div>}
    </div>
  )
}

export function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  useEffect(() => {
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    document.addEventListener('keydown', esc)
    return () => document.removeEventListener('keydown', esc)
  }, [onClose])
  return (
    <div className="modal" role="dialog" aria-label={title} onMouseDown={e => { if (e.target === e.currentTarget) onClose() }}>
      <div className="card wide">
        <div className="row"><h3>{title}</h3><button className="ghost icon" aria-label="close" onClick={onClose}>×</button></div>
        {children}
      </div>
    </div>
  )
}

/** 第一句（截一行用）。 */
export function firstSentence(s: string): string {
  const t = s.trim().split('\n')[0]
  const m = t.match(/^(.+?[。．.!?！？])(\s|$)/)
  return m ? m[1] : t
}
