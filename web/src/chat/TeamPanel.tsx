import { t as tr } from '../i18n'
import { useCallback, useEffect, useState } from 'react'
import { subscribe, type User } from '../api'
import * as chat from './api'
import * as ag from '../agents/api'
import { RoomRoleDialog } from './RoomRole'

type Activity = { text: string; tools: string[] }

/** 右欄：成員、各角色在本群的狀態、工作中串流、停止按鈕、權限卡片。 */
export function TeamPanel({ user, roomId, members }: { user: User; roomId: number; members: chat.Member[] }) {
  const [lanes, setLanes] = useState<Record<string, chat.LaneInfo>>({})
  const [activity, setActivity] = useState<Record<string, Activity>>({})
  const [err, setErr] = useState('')
  const [invite, setInvite] = useState('')
  const [roles, setRoles] = useState<chat.RoleInfo[]>([])
  const [open, setOpen] = useState<string | null>(null)
  const [ov, setOv] = useState<Record<string, ag.RoomOverrideView>>({})
  // 群級 lead：owner 在這裡指定／撤換，只影響本群
  const [lead, setLeadState] = useState<string | null>(null)
  const loadLead = useCallback(() => { void chat.room(roomId).then(r => setLeadState(r.lead ?? null)).catch(() => undefined) }, [roomId])
  useEffect(loadLead, [loadLead])
  useEffect(() => subscribe(e => { if (e.kind === 'chat.room.updated' && e.payload.room?.id === roomId) loadLead() }), [roomId, loadLead])

  const load = useCallback(async () => {
    try {
      const l = await chat.lanes(roomId)
      setLanes(Object.fromEntries(l.lanes.map(x => [x.role, x])))
    } catch (e) { setErr(String(e)) }
  }, [roomId])
  useEffect(() => { void load(); void chat.roles().then(setRoles).catch(() => undefined) }, [load])
  const roleNames = members.filter(m => m.kind === 'role').map(m => m.member).join(',')
  const loadOv = useCallback(() => {
    if (user.rank === 'agent') return
    for (const r of roleNames.split(',').filter(Boolean)) void ag.roomOverride(roomId, r).then(v => setOv(o => ({ ...o, [r]: v }))).catch(() => undefined)
  }, [roomId, roleNames, user.rank])
  useEffect(loadOv, [loadOv])
  useEffect(() => subscribe(e => { if (e.kind.startsWith('runtime.roster.')) loadOv() }), [loadOv])

  useEffect(() => subscribe(e => {
    if (!e.kind.startsWith('runtime.') || e.payload.lane?.room !== roomId) return
    const role: string = e.payload.lane.role
    const ev = e.payload.event
    switch (e.kind) {
      case 'runtime.agent.status':
        setLanes(v => ({ ...v, [role]: { ...(v[role] ?? { role, pending_permissions: [] }), status: ev.status } }))
        break
      case 'runtime.turn.started':
        setActivity(v => ({ ...v, [role]: { text: '', tools: [] } }))
        break
      case 'runtime.agent.stream':
        if (ev.type === 'message_chunk') setActivity(v => ({ ...v, [role]: { text: (v[role]?.text ?? '') + ev.text, tools: v[role]?.tools ?? [] } }))
        break
      case 'runtime.agent.tool_call':
        setActivity(v => ({ ...v, [role]: { text: v[role]?.text ?? '', tools: [...(v[role]?.tools ?? []).filter(t => !t.startsWith(ev.id + ' ')), `${ev.id} ${ev.title} (${ev.status})`] } }))
        break
      case 'runtime.permission.requested':
        setLanes(v => ({ ...v, [role]: { ...(v[role] ?? { role, status: 'blocked', pending_permissions: [] }), pending_permissions: [...(v[role]?.pending_permissions ?? []), ev] } }))
        break
      case 'runtime.permission.resolved':
        setLanes(v => v[role] ? { ...v, [role]: { ...v[role], pending_permissions: v[role].pending_permissions.filter(p => p.request_id !== ev.request_id) } } : v)
        break
      case 'runtime.turn.ended': case 'runtime.turn.failed': case 'runtime.turn.incomplete':
        setActivity(v => { const n = { ...v }; delete n[role]; return n })
        break
      case 'runtime.lane.closed':
        setLanes(v => { const n = { ...v }; delete n[role]; return n })
        break
      case 'runtime.lane.blocked':
        setLanes(v => ({ ...v, [role]: { ...(v[role] ?? { role, status: 'offline', pending_permissions: [] }), stuck: e.payload.error } }))
        break
    }
  }), [roomId])
  // 解除前提醒：中斷的那輪可能已做了一半（改檔、發文），解除後排隊的訊息會重新派工
  // 移除成員（agent 工作中後端會拒絕，按鈕也先停用）；歷史訊息保留
  const canManage = user.rank !== 'agent'
  const remove = (member: string) => {
    if (confirm(tr('Remove {member} from this room? Their messages stay; anything still queued for them here is cancelled.', { member }))) void act(() => chat.removeMember(roomId, member))
  }
  const recover = (role: string) => {
    if (confirm(tr('{role} was interrupted mid-turn and may have done part of the work (edited files, posted). Check first, then recover? Queued messages will be dispatched again.', { role }))) void act(() => chat.recover(roomId, role))
  }

  const act = async (f: () => Promise<unknown>) => { setErr(''); try { await f(); await load() } catch (e) { setErr(e instanceof Error ? e.message : String(e)) } }
  const roleMembers = members.filter(m => m.kind === 'role')
  const humans = members.filter(m => m.kind === 'user')
  const candidates = roles.filter(r => !members.some(m => m.member === r.name))

  return (
    <aside className="team">
      <h3>{tr("Agents")}</h3>
      {roleMembers.map(m => {
        const l = lanes[m.member]
        const a = activity[m.member]
        const status = l?.status ?? 'offline'
        return (
          <section key={m.member} className="agent" aria-label={`agent ${m.member}`}>
            <button className="agent-head" aria-label={`settings for ${m.member}`} title={tr('Agent, model and permissions in this room')} onClick={() => setOpen(m.member)}>
              <span className="row"><span><span className={`dot ${status}`} /> {m.member}{lead === m.member && <span className="tag">{tr('room lead')}</span>}</span><span className="muted small">{tr(status)}</span></span>
              {ov[m.member] && <span className="row wrap small">
                <span className="muted one-line">{ov[m.member].effective.model ?? tr('default model')}</span>
                {Object.keys(ov[m.member].override ?? {}).length > 0 && <span className="tag">{tr('this room')}</span>}
                {ov[m.member].yolo && <span className="tag warn">YOLO</span>}
              </span>}
            </button>
            {(status === 'working' || status === 'blocked') && <button className="danger small" onClick={() => void act(() => chat.cancel(roomId, m.member))}>{tr("Stop")}</button>}
            {canManage && <button className="ghost small remove-member" disabled={status === 'working' || status === 'blocked'} title={status === 'working' || status === 'blocked' ? tr('Stop it first') : undefined} onClick={() => remove(m.member)}>{tr("Remove from room")}</button>}
            {l?.stuck && (
              <div className="perm card" role="group" aria-label="stuck lane">
                <div>⛔ <strong>{m.member}</strong> {tr("is stuck; new messages stay queued.")}</div>
                <div className="muted small one-line" title={l.stuck}>{l.stuck}</div>
                {user.rank === 'owner' && <div className="row wrap"><button className="small" onClick={() => recover(m.member)}>{tr("Recover")}</button></div>}
              </div>
            )}
            {a && (a.tools.length > 0 || a.text) && (
              <details open className="activity"><summary>{tr("working…")}</summary>
                {a.tools.map(t => <div key={t} className="tool">🔧 {t}</div>)}
                {a.text && <pre>{a.text.slice(-500)}</pre>}
              </details>
            )}
            {l?.pending_permissions.map(p => (
              <div key={p.request_id} className="perm card" role="group" aria-label="permission request">
                <div>🔐 <strong>{m.member}</strong> {tr("asks:")}{' '}{p.title || 'permission'}</div>
                <div className="row wrap">
                  {p.options.map(o => <button key={o.id} className={o.kind.startsWith('reject') ? 'danger small' : 'small'} onClick={() => void act(() => chat.answer(roomId, m.member, p.request_id, o.id))}>{o.name}</button>)}
                  <button className="small" onClick={() => void act(() => chat.answer(roomId, m.member, p.request_id, null))}>{tr("Cancel")}</button>
                </div>
              </div>
            ))}
          </section>
        )
      })}
      {roleMembers.length === 0 && <p className="muted">{tr("No agents in this room.")}</p>}
      {user.rank === 'owner' && roleMembers.length > 0 && (
        <label className="row small">{tr("Room lead")}
          <select aria-label="room lead" value={lead ?? ''} onChange={e => void act(async () => { await chat.setLead(roomId, e.target.value || null); loadLead() })}>
            <option value="">{tr("(global setting)")}</option>
            {roleMembers.map(m => <option key={m.member} value={m.member}>{m.member}</option>)}
          </select>
        </label>
      )}
      <h3>{tr("People")}</h3>
      {humans.map(h => (
        <div key={h.member} className="row">
          <span>{h.member}{h.member === user.name ? tr(' (you)') : ''}</span>
          {canManage && h.member !== user.name && <button className="ghost small remove-member" onClick={() => remove(h.member)}>{tr("Remove from room")}</button>}
        </div>
      ))}
      {candidates.length > 0 && (
        <div className="row">
          <select aria-label="invite" value={invite} onChange={e => setInvite(e.target.value)}>
            <option value="">{tr("Invite agent…")}</option>
            {candidates.filter(r => !r.manual).map(r => <option key={r.name} value={r.name}>{r.name}</option>)}
          </select>
          <button disabled={!invite} onClick={() => void act(async () => { await chat.invite(roomId, invite); setInvite('') })}>{tr("Add")}</button>
        </div>
      )}
      {err && <p role="alert" className="err">{err}</p>}
      {open && <RoomRoleDialog roomId={roomId} role={open} onClose={() => setOpen(null)} onChanged={loadOv} />}
    </aside>
  )
}
