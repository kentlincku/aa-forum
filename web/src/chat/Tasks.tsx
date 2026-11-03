import { t as tr } from '../i18n'
import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { subscribe, type User } from '../api'
import * as chat from './api'

const STATES: chat.TaskState[] = ['open', 'claimed', 'blocked', 'done', 'cancelled']
const ICON: Record<chat.TaskState, string> = { open: '○', claimed: '◐', blocked: '⛔', done: '✓', cancelled: '✕' }

const msg = (e: unknown) => (e instanceof Error ? e.message : String(e))

/** 任務即時更新：chat.task.* 事件觸發重新載入。 */
function useTaskEvents(reload: () => void) {
  useEffect(() => subscribe(e => { if (e.kind.startsWith('chat.task.')) reload() }), [reload])
}

function leaseLeft(t: chat.Task) {
  if (t.state !== 'claimed' || !t.lease_until) return ''
  const s = t.lease_until - Math.floor(Date.now() / 1000)
  return s > 0 ? tr('lease {n} min', { n: Math.ceil(s / 60) }) : tr('lease expired')
}

/** 單一任務卡：依身分顯示可做的動作（與後端規則一致，後端仍會再檢查）。 */
export function TaskCard({ t, user, onChanged, showRoom, go }: { t: chat.Task; user: User; onChanged: () => void; showRoom?: string; go?: (p: string) => void }) {
  const [err, setErr] = useState('')
  const [open, setOpen] = useState(false)
  const me = user.name
  const run = async (f: () => Promise<unknown>) => { setErr(''); try { await f(); onChanged() } catch (e) { setErr(msg(e)) } }
  const report = (state: 'done' | 'blocked' | 'cancelled') => {
    const label = state === 'done' ? tr('Result') : state === 'blocked' ? tr('What is blocking?') : tr('Reason to cancel (optional)')
    const r = prompt(label)
    if (r === null) return
    if (state === 'blocked' && !r.trim()) return setErr(tr('Please say what is blocking.'))
    void run(() => chat.reportTask(t.id, state, r))
  }
  // 與後端一致：已指派的 open 任務只給被指派者或發起人（＝接手）
  const reservedOk = !t.assignee || t.assignee === me || t.owner === me
  const canClaim = (t.state === 'open' && reservedOk) || (t.state === 'blocked' && (t.assignee === me || t.owner === me))
  const takeover = !!t.assignee && t.assignee !== me && (t.state === 'open' || t.state === 'blocked')
  const mineClaimed = t.state === 'claimed' && t.assignee === me
  const canCancel = t.owner === me && t.state !== 'done' && t.state !== 'cancelled'
  return (
    <article className={`task ${t.state}`} aria-label={`task ${t.id}`}>
      <div className="row">
        <button className="link task-title" onClick={() => setOpen(o => !o)} aria-expanded={open}>
          <span className="state" title={t.state}>{ICON[t.state]}</span> #{t.id} {t.title}
        </button>
        <span className={`pill ${t.state}`}>{tr(t.state)}</span>
      </div>
      <div className="muted small">
        {showRoom && go ? <><a href={`#/rooms/${t.room}`} onClick={e => { e.preventDefault(); go(`/rooms/${t.room}`) }}>{showRoom}</a> · </> : null}
        {tr("by")}{' '}{t.owner}{t.assignee ? ` → ${t.assignee}${t.state === 'open' ? tr(' (assigned)') : ''}` : tr(' · unassigned')}{leaseLeft(t) ? ` · ${leaseLeft(t)}` : ''}
      </div>
      {open && (
        <div className="task-detail">
          {t.spec && <div className="body">{t.spec}</div>}
          {t.result && <div className="result"><strong>{t.state === 'blocked' ? tr('Blocked:') : tr('Result:')}</strong> {t.result}</div>}
          {t.message && go && <a className="small" href={`#/rooms/${t.room}`} onClick={e => { e.preventDefault(); go(`/rooms/${t.room}`); setTimeout(() => document.getElementById(`m${t.message}`)?.scrollIntoView({ block: 'center' }), 300) }}>{tr("view in room")}</a>}
        </div>
      )}
      <div className="row wrap actions">
        {canClaim && <button className="small" onClick={() => void run(() => chat.claimTask(t.id))}>{takeover ? tr('Take over') : tr('Claim')}</button>}
        {mineClaimed && <button className="small" onClick={() => void run(() => chat.claimTask(t.id))}>{tr("Renew")}</button>}
        {mineClaimed && <button className="small" onClick={() => report('done')}>{tr("Done")}</button>}
        {mineClaimed && <button className="small" onClick={() => report('blocked')}>{tr("Blocked")}</button>}
        {canCancel && <button className="small danger" onClick={() => report('cancelled')}>{tr("Cancel")}</button>}
      </div>
      {err && <p role="alert" className="err">{err}</p>}
    </article>
  )
}

function NewTask({ roomId, members, onDone }: { roomId: number; members: chat.Member[]; onDone: () => void }) {
  const [title, setTitle] = useState('')
  const [spec, setSpec] = useState('')
  const [assignee, setAssignee] = useState('')
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState('')
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true); setErr('')
    try { await chat.createTask(roomId, title, spec, assignee || null); setTitle(''); setSpec(''); setAssignee(''); onDone() }
    catch (x) { setErr(msg(x)) } finally { setBusy(false) }
  }
  return (
    <form className="card new-task" onSubmit={submit} aria-label="new task">
      <input aria-label="task title" placeholder={tr("Task title")} value={title} maxLength={200} onChange={e => setTitle(e.target.value)} autoFocus />
      <textarea aria-label="task spec" placeholder={tr("Details / acceptance criteria (optional)")} rows={3} value={spec} onChange={e => setSpec(e.target.value)} />
      <select aria-label="assignee" value={assignee} onChange={e => setAssignee(e.target.value)}>
        <option value="">{tr("Unassigned (anyone in the room can claim)")}</option>
        {members.map(m => <option key={m.member} value={m.member}>{m.member}{m.kind === 'role' ? tr(' (agent)') : ''}</option>)}
      </select>
      <div className="muted small">{assignee && members.find(m => m.member === assignee)?.kind === 'role' ? `@${assignee} will be woken to work on it.` : tr('Posted in the room as a task message.')}</div>
      {err && <p role="alert" className="err">{err}</p>}
      <div className="row"><button type="button" onClick={onDone}>{tr("Cancel")}</button><button type="submit" disabled={busy || !title.trim()}>{tr("Create task")}</button></div>
    </form>
  )
}

/** 停住的工作：超過門檻沒有任務更新、負責人也沒在跑回合。手動叫醒，不排程。 */
function StalledList({ roomId, readOnly }: { roomId: number; readOnly: boolean }) {
  const [list, setList] = useState<chat.Stalled[]>([])
  const [err, setErr] = useState('')
  const load = useCallback(() => { void chat.stalledTasks(roomId).then(v => { setList(v); setErr('') }).catch(e => setErr(msg(e))) }, [roomId])
  useEffect(load, [load])
  useTaskEvents(load)
  useEffect(() => { const h = setInterval(load, 60_000); return () => clearInterval(h) }, [load])
  if (list.length === 0 && !err) return null
  const wake = (s: chat.Stalled) => { void chat.wakeTask(roomId, s.id).then(load).catch(e => setErr(msg(e))) }
  return (
    <div className="stalled" aria-label="stalled tasks">
      <h4>{tr("Stalled work")}</h4>
      {err && <p role="alert" className="err">{err}</p>}
      {list.map(s => (
        <div key={s.id} className="task">
          <div>{ICON[s.state]} #{s.id} {s.title}{' '}<span className="muted small">{tr("idle {n} min", { n: Math.floor(s.idle_secs / 60) })}</span>
            {s.kind === 'waiting_external' && <span className="badge">{tr("waiting on external")}</span>}</div>
          {!readOnly && <button className="small" disabled={!s.wake_target} title={s.wake_note ?? ''} onClick={() => wake(s)}>
            {s.wake_target ? tr("Wake {r}", { r: s.wake_target }) : tr("No one to wake")}</button>}
          {!s.wake_target && s.wake_note && <span className="muted small"> {s.wake_note}</span>}
        </div>
      ))}
    </div>
  )
}

/** 群內任務板（右欄分頁）。 */
export function RoomTasks({ user, roomId, members, readOnly, go }: { user: User; roomId: number; members: chat.Member[]; readOnly: boolean; go: (p: string) => void }) {
  const [tasks, setTasks] = useState<chat.Task[]>([])
  const [showDone, setShowDone] = useState(false)
  const [creating, setCreating] = useState(false)
  const [err, setErr] = useState('')
  const load = useCallback(() => { void chat.roomTasks(roomId).then(t => { setTasks(t); setErr('') }).catch(e => setErr(msg(e))) }, [roomId])
  useEffect(load, [load])
  useTaskEvents(load)
  const active = tasks.filter(t => t.state === 'open' || t.state === 'claimed' || t.state === 'blocked')
  const closed = tasks.filter(t => t.state === 'done' || t.state === 'cancelled')
  return (
    <div className="tasks" aria-label="room tasks">
      <div className="row"><span className="muted">{active.length} {tr("active ·")}{' '}{closed.length} {tr("closed")}</span>
        {!readOnly && user.rank !== 'viewer' && !creating && <button className="small" onClick={() => setCreating(true)}>{tr("＋ Task")}</button>}</div>
      <StalledList roomId={roomId} readOnly={readOnly} />
      {creating && <NewTask roomId={roomId} members={members} onDone={() => { setCreating(false); load() }} />}
      {err && <p role="alert" className="err">{err}</p>}
      {(['blocked', 'claimed', 'open'] as const).map(s => active.filter(t => t.state === s).map(t => <TaskCard key={t.id} t={t} user={user} onChanged={load} go={go} />))}
      {active.length === 0 && !creating && <p className="muted">{tr("No active tasks.")}</p>}
      {closed.length > 0 && <button className="link" onClick={() => setShowDone(v => !v)}>{showDone ? tr('Hide') : tr('Show')} {tr("closed (")}{closed.length})</button>}
      {showDone && closed.slice().reverse().map(t => <TaskCard key={t.id} t={t} user={user} onChanged={load} go={go} />)}
    </div>
  )
}

/** 全域任務頁：我所在所有群的任務，可依狀態與「我的」篩選。 */
export function TaskBoard({ user, go }: { user: User; go: (p: string) => void }) {
  const [tasks, setTasks] = useState<chat.Task[]>([])
  const [rooms, setRooms] = useState<Record<number, string>>({})
  const [mine, setMine] = useState(false)
  const [state, setState] = useState<chat.TaskState | ''>('')
  const [err, setErr] = useState('')
  const load = useCallback(() => {
    void Promise.all([chat.allTasks(mine, state || undefined), chat.rooms()])
      .then(([t, r]) => { setTasks(t); setRooms(Object.fromEntries(r.map(x => [x.id, x.name]))); setErr('') })
      .catch(e => setErr(msg(e)))
  }, [mine, state])
  useEffect(load, [load])
  useTaskEvents(load)
  const cols = state ? [state] : (['open', 'claimed', 'blocked', 'done'] as chat.TaskState[])
  return (
    <main className="page board">
      <header className="row">
        <h2>{tr("Tasks")}</h2>
        <div className="row filters">
          <label className="inline"><input type="checkbox" checked={mine} onChange={e => setMine(e.target.checked)} /> {tr("Mine only")}</label>
          <select aria-label="state filter" value={state} onChange={e => setState(e.target.value as chat.TaskState | '')}>
            <option value="">{tr("All states")}</option>
            {STATES.map(s => <option key={s} value={s}>{tr(s)}</option>)}
          </select>
        </div>
      </header>
      {err && <p role="alert" className="err">{err}</p>}
      <div className="columns" style={{ gridTemplateColumns: `repeat(${cols.length}, minmax(220px, 1fr))` }}>
        {cols.map(c => {
          const ts = tasks.filter(t => t.state === c)
          return (
            <section key={c} className="column" aria-label={`${c} tasks`}>
              <h3>{ICON[c]} {tr(c)} <span className="muted">{ts.length}</span></h3>
              {ts.map(t => <TaskCard key={t.id} t={t} user={user} onChanged={load} showRoom={rooms[t.room] ?? tr('room {n}', { n: t.room })} go={go} />)}
              {ts.length === 0 && <p className="muted small">—</p>}
            </section>
          )
        })}
      </div>
      <p className="muted small">{tr("Create tasks inside a room (Tasks tab). Agents use")}{' '}<code>{tr("aaf task …")}</code>.</p>
    </main>
  )
}
