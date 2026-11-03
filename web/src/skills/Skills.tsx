import { t as tr } from '../i18n'
import { useCallback, useEffect, useState } from 'react'
import type { User } from '../api'
import * as chat from '../chat/api'
import * as sk from './api'
import { firstSentence } from '../ui/bits'

const msg = (e: unknown) => (e instanceof Error ? e.message : String(e))
type Tab = 'library' | 'packs' | 'assign'

/** Skills 頁：skill 庫（編輯、版本、還原）、分類包、指派給角色或群、預覽。人可改；agent 只讀。 */
export function SkillsPage({ user }: { user: User }) {
  const [data, setData] = useState<sk.Listing | null>(null)
  const [tab, setTab] = useState<Tab>('library')
  const [err, setErr] = useState('')
  const load = useCallback(() => { void sk.list().then(d => { setData(d); setErr('') }).catch(e => setErr(msg(e))) }, [])
  useEffect(load, [load])
  const canEdit = user.rank === 'owner' || user.rank === 'human'
  return (
    <main className="page skills">
      <header className="row"><h2>{tr("Skills")}</h2>
        <div className="tabs" role="tablist">
          {(['library', 'packs', 'assign'] as Tab[]).map(t => <button key={t} role="tab" aria-selected={tab === t} className={tab === t ? 'active' : ''} onClick={() => setTab(t)}>{t === 'library' ? tr('Library') : t === 'packs' ? tr('Packs') : tr('Assign')}</button>)}
        </div>
      </header>
      {data && <details className="info"><summary className="muted small">ⓘ {tr('How skills are loaded')}</summary><p className="muted small">{tr("Stored in")}{' '}{data.dir}{tr(". Assigned skills are sent to an agent at the start of its session and again when they change; other skills are listed with their path.")}</p></details>}
      {err && <p role="alert" className="err">{err}</p>}
      {data && tab === 'library' && <Library data={data} canEdit={canEdit} reload={load} />}
      {data && tab === 'packs' && <Packs data={data} canEdit={canEdit} reload={load} />}
      {data && tab === 'assign' && <AssignView data={data} canEdit={canEdit} reload={load} />}
    </main>
  )
}

function Library({ data, canEdit, reload }: { data: sk.Listing; canEdit: boolean; reload: () => void }) {
  const [sel, setSel] = useState<string | null>(data.skills[0]?.id ?? null)
  const [creating, setCreating] = useState(false)
  return (
    <div className="skill-lib">
      <aside className="skill-list" aria-label="skills">
        {canEdit && <button onClick={() => { setCreating(true); setSel(null) }}>{tr("＋ New skill")}</button>}
        {data.skills.map(s => (
          <button key={s.id} className={`skill-item ${sel === s.id ? 'active' : ''}`} onClick={() => { setSel(s.id); setCreating(false) }}>
            <strong>{s.id}</strong>
            <span className="muted small one-line" title={s.description}>{s.description ? firstSentence(s.description) : tr('(no description)')}</span>
            {s.used_by.length > 0 && <span className="pill">{s.used_by.length} {tr("in use")}</span>}
          </button>
        ))}
        {data.skills.length === 0 && <p className="muted">{tr("No skills yet.")}</p>}
      </aside>
      {creating ? <NewSkill onDone={id => { setCreating(false); reload(); if (id) setSel(id) }} />
        : sel ? <Editor key={sel} id={sel} info={data.skills.find(s => s.id === sel)} canEdit={canEdit} onChanged={reload} onDeleted={() => { setSel(null); reload() }} />
        : <div className="muted">{tr("Select a skill.")}</div>}
    </div>
  )
}

const TEMPLATE = '---\nname: my-skill\ndescription: One line about when to use this skill.\n---\n\n# Steps\n\n1. \n'

function NewSkill({ onDone }: { onDone: (id?: string) => void }) {
  const [id, setId] = useState('')
  const [content, setContent] = useState(TEMPLATE)
  const [err, setErr] = useState('')
  const go = async () => { try { await sk.create(id, content); onDone(id) } catch (e) { setErr(msg(e)) } }
  return (
    <section className="card skill-editor" aria-label="new skill">
      <input aria-label="skill id" placeholder={tr("skill-id (lowercase, digits, - _)")} value={id} onChange={e => setId(e.target.value.toLowerCase())} autoFocus />
      <textarea aria-label="skill content" rows={18} value={content} onChange={e => setContent(e.target.value)} spellCheck={false} />
      {err && <p role="alert" className="err">{err}</p>}
      <div className="row"><button onClick={() => onDone()}>{tr("Cancel")}</button><button disabled={!id} onClick={() => void go()}>{tr("Create")}</button></div>
    </section>
  )
}

function Editor({ id, info, canEdit, onChanged, onDeleted }: { id: string; info?: sk.SkillInfo; canEdit: boolean; onChanged: () => void; onDeleted: () => void }) {
  const [text, setText] = useState('')
  const [orig, setOrig] = useState('')
  const [note, setNote] = useState('')
  const [hist, setHist] = useState<{ versions: sk.Version[]; log: sk.LogEntry[] } | null>(null)
  const [view, setView] = useState<{ v: string; content: string } | null>(null)
  const [status, setStatus] = useState('')
  const [err, setErr] = useState('')
  const load = useCallback(() => {
    void sk.read(id).then(r => { setText(r.content); setOrig(r.content) }).catch(e => setErr(msg(e)))
    void sk.history(id).then(setHist).catch(() => undefined)
  }, [id])
  useEffect(load, [load])
  const dirty = text !== orig
  const run = async (f: () => Promise<unknown>, done: string) => { setErr(''); try { await f(); setStatus(done); load(); onChanged() } catch (e) { setErr(msg(e)) } }
  return (
    <section className="skill-editor" aria-label={`skill ${id}`}>
      <div className="row"><h3>{id}</h3><span className="muted small">{info ? `${info.bytes} bytes · ${info.packs.length ? 'packs: ' + info.packs.join(', ') : 'no pack'}` : ''}</span></div>
      {info && info.used_by.length > 0 && <p className="muted small">{tr("Used by:")}{' '}{info.used_by.join(' · ')}</p>}
      <textarea aria-label="skill content" rows={20} value={text} readOnly={!canEdit} onChange={e => setText(e.target.value)} spellCheck={false} />
      {canEdit && (
        <div className="row wrap">
          <input aria-label="change note" placeholder={tr("What changed? (optional)")} value={note} onChange={e => setNote(e.target.value)} />
          <button disabled={!dirty} onClick={() => void run(async () => { await sk.save(id, text, note); setNote('') }, 'Saved')}>{tr("Save")}</button>
          <button disabled={!dirty} onClick={() => setText(orig)}>{tr("Revert")}</button>
          <button className="danger" onClick={() => { if (confirm(tr('Delete skill "{id}"? A copy stays in history.', { id }))) void sk.remove(id).then(onDeleted).catch(e => setErr(msg(e))) }}>{tr("Delete")}</button>
          <span className="muted small">{dirty ? 'unsaved changes' : status}</span>
        </div>
      )}
      {!canEdit && <p className="muted small">{tr("Agents can read skills but not change them.")}</p>}
      {err && <p role="alert" className="err">{err}</p>}
      {hist && (
        <details className="history">
          <summary>{tr("History (")}{hist.versions.length} {tr("versions)")}</summary>
          <ul>
            {hist.versions.map(v => (
              <li key={v.version} className="row">
                <span>{v.version} <span className="muted small">{v.bytes} B</span></span>
                <span><button className="link" onClick={() => void sk.version(id, v.version).then(r => setView({ v: v.version, content: r.content }))}>{tr("view")}</button>
                  {canEdit && <button className="link" onClick={() => { if (confirm(tr('Restore {version}? The current text is saved to history first.', { version: v.version }))) void run(() => sk.restore(id, v.version), `Restored ${v.version}`) }}>{tr("restore")}</button>}</span>
              </li>
            ))}
          </ul>
          <div className="log">{hist.log.slice().reverse().map((l, i) => <div key={i} className="muted small">{new Date(l.at * 1000).toLocaleString()} · {l.actor} · {l.action}{l.note ? ` · ${l.note}` : ''}</div>)}</div>
        </details>
      )}
      {view && <div className="modal" role="dialog" aria-label="version"><div className="card wide"><h3>{id} @ {view.v}</h3><pre className="skill-pre">{view.content}</pre><div className="row"><button onClick={() => setView(null)}>{tr("Close")}</button></div></div></div>}
    </section>
  )
}

function Packs({ data, canEdit, reload }: { data: sk.Listing; canEdit: boolean; reload: () => void }) {
  const [name, setName] = useState('')
  const [err, setErr] = useState('')
  const ids = data.skills.map(s => s.id)
  const toggle = async (pack: string, id: string, on: boolean) => {
    const cur = data.packs[pack] ?? []
    try { await sk.setPack(pack, on ? [...cur, id] : cur.filter(x => x !== id)); reload() } catch (e) { setErr(msg(e)) }
  }
  return (
    <div className="packs">
      {Object.entries(data.packs).map(([p, members]) => (
        <section key={p} className="card" aria-label={`pack ${p}`}>
          <div className="row"><strong>{p}</strong>{canEdit && <button className="danger small" onClick={() => void sk.deletePack(p).then(reload).catch(e => setErr(msg(e)))}>{tr("Delete pack")}</button>}</div>
          <div className="row wrap">{ids.map(id => <label key={id} className="inline"><input type="checkbox" disabled={!canEdit} checked={members.includes(id)} onChange={e => void toggle(p, id, e.target.checked)} /> {id}</label>)}</div>
        </section>
      ))}
      {Object.keys(data.packs).length === 0 && <p className="muted">{tr("No packs. A pack groups skills so you can assign them together.")}</p>}
      {canEdit && <div className="row"><input aria-label="new pack name" placeholder={tr("new pack name")} value={name} onChange={e => setName(e.target.value)} /><button disabled={!name.trim()} onClick={() => void sk.setPack(name.trim(), []).then(() => { setName(''); reload() }).catch(e => setErr(msg(e)))}>{tr("Add pack")}</button></div>}
      {err && <p role="alert" className="err">{err}</p>}
    </div>
  )
}

function AssignView({ data, canEdit, reload }: { data: sk.Listing; canEdit: boolean; reload: () => void }) {
  const [roles, setRoles] = useState<chat.RoleInfo[]>([])
  const [rooms, setRooms] = useState<chat.Room[]>([])
  const [target, setTarget] = useState<{ kind: 'role' | 'room'; key: string } | null>(null)
  const [pv, setPv] = useState<{ role: string; room: string; out: { skills: string[]; context: string } | null }>({ role: '', room: '', out: null })
  const [err, setErr] = useState('')
  useEffect(() => { void chat.roles().then(r => setRoles(r.filter(x => !x.manual))).catch(() => undefined); void chat.rooms().then(setRooms).catch(() => undefined) }, [])
  const cur: sk.Assign = target ? (target.kind === 'role' ? data.roles[target.key] : data.rooms[target.key]) ?? { skills: [], packs: [] } : { skills: [], packs: [] }
  const update = async (a: sk.Assign) => { if (!target) return; try { await sk.assign(target.kind, target.key, a); reload() } catch (e) { setErr(msg(e)) } }
  const flip = (list: string[], v: string, on: boolean) => on ? [...list, v] : list.filter(x => x !== v)
  const summary = (a?: sk.Assign) => a ? [...a.skills, ...a.packs.map(p => `pack:${p}`)].join(', ') || '—' : '—'
  return (
    <div className="assign">
      <div className="assign-targets">
        <h3>{tr("Roles")}</h3>
        {roles.map(r => <button key={r.name} className={`skill-item ${target?.kind === 'role' && target.key === r.name ? 'active' : ''}`} onClick={() => setTarget({ kind: 'role', key: r.name })}><strong>{r.name}</strong><span className="muted small">{summary(data.roles[r.name])}</span></button>)}
        <h3>{tr("Rooms")}</h3>
        {rooms.map(r => <button key={r.id} className={`skill-item ${target?.kind === 'room' && target.key === String(r.id) ? 'active' : ''}`} onClick={() => setTarget({ kind: 'room', key: String(r.id) })}><strong>{r.name}</strong><span className="muted small">{summary(data.rooms[String(r.id)])}</span></button>)}
      </div>
      <div>
        {target ? (
          <section className="card" aria-label="assignment">
            <h3>{target.kind === 'role' ? tr('Role {key}', { key: target.key }) : tr('Room {name}', { name: rooms.find(r => String(r.id) === target.key)?.name ?? target.key })}</h3>
            <p className="muted small">{target.kind === 'role' ? tr('Loaded for this role in every room.') : tr('Loaded for every agent in this room.')} {tr("Order: role skills → role packs → room skills → room packs; duplicates load once.")}</p>
            <strong>{tr("Skills")}</strong>
            <div className="row wrap">{data.skills.map(s => <label key={s.id} className="inline"><input type="checkbox" disabled={!canEdit} checked={cur.skills.includes(s.id)} onChange={e => void update({ ...cur, skills: flip(cur.skills, s.id, e.target.checked) })} /> {s.id}</label>)}</div>
            <strong>{tr("Packs")}</strong>
            <div className="row wrap">{Object.keys(data.packs).map(p => <label key={p} className="inline"><input type="checkbox" disabled={!canEdit} checked={cur.packs.includes(p)} onChange={e => void update({ ...cur, packs: flip(cur.packs, p, e.target.checked) })} /> {p}</label>)}
              {Object.keys(data.packs).length === 0 && <span className="muted small">{tr("none")}</span>}</div>
          </section>
        ) : <p className="muted">{tr("Pick a role or room to assign skills.")}</p>}
        <section className="card" aria-label="preview">
          <h3>{tr("Preview")}</h3>
          <div className="row">
            <select aria-label="preview role" value={pv.role} onChange={e => setPv({ ...pv, role: e.target.value, out: null })}><option value="">{tr("role…")}</option>{roles.map(r => <option key={r.name}>{r.name}</option>)}</select>
            <select aria-label="preview room" value={pv.room} onChange={e => setPv({ ...pv, room: e.target.value, out: null })}><option value="">{tr("no room")}</option>{rooms.map(r => <option key={r.id} value={r.id}>{r.name}</option>)}</select>
            <button disabled={!pv.role} onClick={() => void sk.preview(pv.role, pv.room ? Number(pv.room) : null).then(out => setPv(v => ({ ...v, out }))).catch(e => setErr(msg(e)))}>{tr("Preview")}</button>
          </div>
          {pv.out && <><div className="small">{tr("Loads:")}{' '}{pv.out.skills.join(' → ') || 'nothing assigned'}</div><pre className="skill-pre" aria-label="preview text">{pv.out.context || '(nothing is sent)'}</pre></>}
        </section>
        {err && <p role="alert" className="err">{err}</p>}
      </div>
    </div>
  )
}
