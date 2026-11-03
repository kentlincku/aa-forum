import { t as tr } from '../i18n'
import { useCallback, useEffect, useRef, useState } from 'react'
import { subscribe, type User } from '../api'
import { humanSize } from '../chat/api'
import * as files from './api'

type Up = { key: string; name: string; progress: number; done?: files.Uploaded; err?: string }

/** Files 頁：瀏覽、搜尋、下載 outputs（與唯讀舊資料區），上傳到 outputs/<日期>/（Owner）。 */
export function FilesPage({ user }: { user: User }) {
  const [spaces, setSpaces] = useState<files.Space[]>([])
  const [limit, setLimit] = useState<number | null>(null)
  const [space, setSpace] = useState('outputs')
  const [q, setQ] = useState('')
  const [query, setQuery] = useState('')
  const [offset, setOffset] = useState(0)
  const [data, setData] = useState<files.Listing | null>(null)
  const [ups, setUps] = useState<Up[]>([])
  const [drag, setDrag] = useState(false)
  const [err, setErr] = useState('')
  const picker = useRef<HTMLInputElement>(null)
  const owner = user.rank === 'owner'

  useEffect(() => { if (owner) void files.me().then(m => { setSpaces(m.spaces); setLimit(m.upload_limit) }).catch(e => setErr(String(e))) }, [owner])
  const load = useCallback(() => {
    if (!owner) return
    void files.list(space, query, offset).then(d => { setData(d); setErr('') }).catch(e => setErr(e instanceof Error ? e.message : String(e)))
  }, [owner, space, query, offset])
  useEffect(load, [load])
  useEffect(() => subscribe(e => { if (e.kind === 'files.uploaded') load() }), [load])
  // 輸入停 300ms 才搜尋
  useEffect(() => { const t = setTimeout(() => { setOffset(0); setQuery(q.trim()) }, 300); return () => clearTimeout(t) }, [q])

  if (!owner) return <main className="page"><h2>{tr("Files")}</h2><p className="muted">{tr("The Files page is available to the owner only.")}</p></main>
  const cur = spaces.find(s => s.id === space)
  const add = (list: FileList | File[]) => {
    for (const f of Array.from(list)) {
      const key = `${Date.now()}-${Math.random()}`
      if (limit && f.size > limit) { setUps(v => [...v, { key, name: f.name, progress: 0, err: `over the ${Math.round(limit / 1048576)} MB limit` }]); continue }
      setUps(v => [...v, { key, name: f.name, progress: 0 }])
      void files.upload(space, f, p => setUps(v => v.map(x => x.key === key ? { ...x, progress: p } : x)))
        .then(done => { setUps(v => v.map(x => x.key === key ? { ...x, done, progress: 1 } : x)); load() })
        .catch(e => setUps(v => v.map(x => x.key === key ? { ...x, err: e instanceof Error ? e.message : String(e) } : x)))
    }
  }
  const copy = (t: string) => { void navigator.clipboard?.writeText(t) }
  return (
    <main className={`page files ${drag ? 'drag' : ''}`}
      onDragOver={e => { if (cur?.writable && e.dataTransfer.types.includes('Files')) { e.preventDefault(); setDrag(true) } }}
      onDragLeave={() => setDrag(false)}
      onDrop={e => { if (cur?.writable && e.dataTransfer.files.length) { e.preventDefault(); setDrag(false); add(e.dataTransfer.files) } }}>
      <header className="row">
        <h2>{tr("Files")}</h2>
        <div className="row filters">
          {spaces.length > 1 && (
            <select aria-label="space" value={space} onChange={e => { setSpace(e.target.value); setOffset(0) }}>
              {spaces.map(s => <option key={s.id} value={s.id}>{s.label}</option>)}
            </select>
          )}
          <input aria-label="search files" placeholder={tr("Search path…")} value={q} onChange={e => setQ(e.target.value)} />
          {cur?.writable && <><button onClick={() => picker.current?.click()}>{tr("Upload…")}</button>
            <input ref={picker} type="file" multiple hidden aria-label="upload picker" onChange={e => { if (e.target.files) add(e.target.files); e.target.value = '' }} /></>}
        </div>
      </header>
      {cur && <p className="muted small">{cur.path}{cur.exists ? '' : tr(' (not created yet)')}{cur.writable ? tr(' · uploads go to <date>/ · drop files here') + (limit ? tr(' · max {n} MB each', { n: Math.round(limit / 1048576) }) : '') : tr(' · read-only')}</p>}
      {ups.length > 0 && (
        <div className="uploads" aria-label="uploads">
          {ups.map(u => (
            <div key={u.key} className={`chip ${u.err ? 'bad' : ''}`}>
              {u.err ? '✕' : u.done ? '✓' : '↑'} {u.name}
              {!u.done && !u.err && <> · {Math.round(u.progress * 100)}%</>}
              {u.done && <> → {u.done.file.rel}{u.done.file.renamed ? ' (renamed)' : ''} <button className="link" onClick={() => copy(u.done!.file.path)}>{tr("copy path")}</button></>}
              {u.err && <> · {u.err}</>}
            </div>
          ))}
          <button className="link" onClick={() => setUps(v => v.filter(x => !x.done && !x.err))}>{tr("clear")}</button>
        </div>
      )}
      {err && <p role="alert" className="err">{err}</p>}
      {data && (
        <>
          <table className="file-table">
            <thead><tr><th>{tr("Path")}</th><th>{tr("Size")}</th><th>{tr("Modified")}</th><th /></tr></thead>
            <tbody>
              {data.files.map(f => (
                <tr key={f.rel}>
                  <td><a href={files.downloadUrl(space, f.rel)} download>{f.rel}</a></td>
                  <td className="num">{humanSize(f.size)}</td>
                  <td>{new Date(f.modified * 1000).toLocaleString()}</td>
                  <td><button className="link" title={tr("Copy absolute path")} onClick={() => copy(`${cur?.path ?? ''}/${f.rel}`)}>{tr("copy path")}</button></td>
                </tr>
              ))}
              {data.files.length === 0 && <tr><td colSpan={4} className="muted">{query ? tr('No matches.') : tr('No files yet.')}</td></tr>}
            </tbody>
          </table>
          <div className="row">
            <span className="muted small">{data.total === 0 ? '' : `${data.offset + 1}–${data.offset + data.files.length} of ${data.total}`}{data.partial ? ' (listing truncated)' : ''}</span>
            <span className="row">
              <button disabled={data.offset === 0} onClick={() => setOffset(Math.max(0, data.offset - data.limit))}>{tr("Prev")}</button>
              <button disabled={data.offset + data.files.length >= data.total} onClick={() => setOffset(data.offset + data.limit)}>{tr("Next")}</button>
            </span>
          </div>
        </>
      )}
    </main>
  )
}
