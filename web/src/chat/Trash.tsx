import { t as tr } from '../i18n'
import { useCallback, useEffect, useState } from 'react'
import { subscribe } from '../api'
import * as chat from './api'

/** 垃圾桶：已刪除的群，可恢復或永久刪除（需輸入群名確認）。 */
export function Trash({ go }: { go: (p: string) => void }) {
  const [list, setList] = useState<chat.Room[]>([])
  const [err, setErr] = useState('')
  const [purging, setPurging] = useState<chat.Room | null>(null)
  const load = useCallback(async () => { try { setList(await chat.rooms('deleted')) } catch (e) { setErr(e instanceof Error ? e.message : String(e)) } }, [])
  useEffect(() => { void load() }, [load])
  useEffect(() => subscribe(e => { if (e.kind.startsWith('chat.room.')) void load() }), [load])
  const restore = async (r: chat.Room) => { try { await chat.setState(r.id, 'restore'); go(`/rooms/${r.id}`) } catch (e) { setErr(String(e)) } }
  return (
    <main className="page">
      <h2>{tr("Trash")}</h2>
      <p className="muted">{tr("Rooms here are purged automatically after the retention period (default 30 days).")}</p>
      {err && <p role="alert" className="err">{err}</p>}
      <table>
        <thead><tr><th>{tr("Room")}</th><th>{tr("Deleted by")}</th><th>{tr("Deleted at")}</th><th /></tr></thead>
        <tbody>
          {list.map(r => (
            <tr key={r.id}>
              <td>{r.name}</td><td>{r.deleted_by}</td><td>{r.deleted_at ? new Date(r.deleted_at * 1000).toLocaleString() : ''}</td>
              <td className="row"><button onClick={() => void restore(r)}>{tr("Restore")}</button><button className="danger" onClick={() => setPurging(r)}>{tr("Delete forever")}</button></td>
            </tr>
          ))}
          {list.length === 0 && <tr><td colSpan={4} className="muted">{tr("Trash is empty.")}</td></tr>}
        </tbody>
      </table>
      {purging && <PurgeDialog room={purging} onClose={() => { setPurging(null); void load() }} />}
    </main>
  )
}

function PurgeDialog({ room, onClose }: { room: chat.Room; onClose: () => void }) {
  const [name, setName] = useState('')
  const [dropTranscripts, setDropTranscripts] = useState(false)
  const [err, setErr] = useState('')
  const go = async () => { try { await chat.purge(room.id, name, dropTranscripts); onClose() } catch (e) { setErr(e instanceof Error ? e.message : String(e)) } }
  return (
    <div className="modal" role="dialog" aria-label="Delete forever">
      <div className="card">
        <h3>{tr("Delete “")}{room.name}{tr("” forever?")}</h3>
        <p>{tr("Messages, members and agent sessions are removed. The audit record is kept. This cannot be undone.")}</p>
        <label className="inline"><input type="checkbox" aria-label="also delete transcripts" checked={dropTranscripts} onChange={e => setDropTranscripts(e.target.checked)} /> {tr("Also delete exported transcripts (kept by default)")}</label>
        <label>{tr("Type the room name to confirm")}<input aria-label="confirm name" value={name} onChange={e => setName(e.target.value)} autoFocus /></label>
        {err && <p role="alert" className="err">{err}</p>}
        <div className="row"><button onClick={onClose}>{tr("Cancel")}</button><button className="danger" disabled={name !== room.name} onClick={() => void go()}>{tr("Delete forever")}</button></div>
      </div>
    </div>
  )
}
