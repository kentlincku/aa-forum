import { t as tr } from '../i18n'
import { useEffect, useState, type FormEvent, type ReactNode } from 'react'
import * as chat from './api'

/** 命中詞加粗（純文字切片，不用 innerHTML）。 */
function mark(text: string, terms: string[]): ReactNode[] {
  const ts = terms.filter(Boolean).map(t => t.toLowerCase())
  if (!ts.length) return [text]
  const out: ReactNode[] = []
  const lower = text.toLowerCase()
  let i = 0
  while (i < text.length) {
    let best = -1, len = 0
    for (const t of ts) { const p = lower.indexOf(t, i); if (p >= 0 && (best < 0 || p < best)) { best = p; len = t.length } }
    if (best < 0) { out.push(text.slice(i)); break }
    if (best > i) out.push(text.slice(i, best))
    out.push(<mark key={best}>{text.slice(best, best + len)}</mark>)
    i = best + len
  }
  return out
}

/** 搜尋頁：關鍵字（"片語"、多詞 AND）＋篩選；點結果跳到該則訊息。 */
export function SearchPage({ go, initial }: { go: (p: string) => void; initial: string }) {
  const [q, setQ] = useState(initial)
  const [rooms, setRooms] = useState<chat.Room[]>([])
  const [room, setRoom] = useState('')
  const [from, setFrom] = useState('')
  const [mentions, setMentions] = useState(false)
  const [hasFile, setHasFile] = useState(false)
  const [days, setDays] = useState('')
  const [res, setRes] = useState<{ hits: chat.Hit[]; more: boolean; terms: string[] } | null>(null)
  const [err, setErr] = useState('')
  const [busy, setBusy] = useState(false)
  useEffect(() => { void chat.rooms().then(setRooms).catch(() => undefined) }, [])
  const opts = (before?: number): chat.SearchOpts => ({
    q: q.trim(), room: room ? Number(room) : undefined, from: from.trim() || undefined, mentions, has_file: hasFile,
    since: days ? Math.floor(Date.now() / 1000) - Number(days) * 86400 : undefined, before,
  })
  const run = async (before?: number) => {
    setBusy(true); setErr('')
    try {
      const r = await chat.search(opts(before))
      setRes(v => before && v ? { ...r, hits: [...v.hits, ...r.hits] } : r)
    } catch (e) { setErr(e instanceof Error ? e.message : String(e)) } finally { setBusy(false) }
  }
  useEffect(() => { if (initial.trim()) void run() }, []) // eslint-disable-line react-hooks/exhaustive-deps
  const submit = (e: FormEvent) => { e.preventDefault(); void run() }
  return (
    <main className="page search">
      <h2>{tr("Search")}</h2>
      <form className="row wrap filters" onSubmit={submit}>
        <input aria-label="search query" placeholder={tr("words, \"exact phrase\"")} value={q} onChange={e => setQ(e.target.value)} autoFocus />
        <select aria-label="room filter" value={room} onChange={e => setRoom(e.target.value)}><option value="">{tr("All my rooms")}</option>{rooms.map(r => <option key={r.id} value={r.id}>{r.name}</option>)}</select>
        <input aria-label="from" placeholder={tr("from (name)")} value={from} onChange={e => setFrom(e.target.value)} />
        <select aria-label="date filter" value={days} onChange={e => setDays(e.target.value)}><option value="">{tr("Any time")}</option><option value="1">{tr("Last 24 h")}</option><option value="7">{tr("Last 7 days")}</option><option value="30">{tr("Last 30 days")}</option></select>
        <label className="inline"><input type="checkbox" checked={mentions} onChange={e => setMentions(e.target.checked)} /> {tr("mentions me")}</label>
        <label className="inline"><input type="checkbox" checked={hasFile} onChange={e => setHasFile(e.target.checked)} /> {tr("has file")}</label>
        <button type="submit" disabled={busy}>{tr("Search")}</button>
      </form>
      {err && <p role="alert" className="err">{err}</p>}
      {res && (
        <div className="hits" aria-label="results">
          {res.hits.map(h => (
            <button key={h.id} className="hit" onClick={() => go(`/rooms/${h.room}?m=${h.id}`)}>
              <div className="muted small">{h.room_name} · {h.author_kind === 'system' ? 'system' : h.author} · {new Date(h.created_at * 1000).toLocaleString()} · #{h.id}</div>
              <div>{mark(h.snippet, res.terms)}</div>
            </button>
          ))}
          {res.hits.length === 0 && <p className="muted">{tr("No messages found.")}</p>}
          {res.more && <button disabled={busy} onClick={() => void run(res.hits.at(-1)?.id)}>{tr("Load older")}</button>}
        </div>
      )}
    </main>
  )
}
