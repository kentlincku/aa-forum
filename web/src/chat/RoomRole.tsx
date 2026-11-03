import { t as tr } from '../i18n'
import { useEffect, useState } from 'react'
import * as ag from '../agents/api'
import { AccountPick, EffortPick } from '../agents/Agents'
import { Modal } from '../ui/bits'

const msg = (e: unknown) => (e instanceof Error ? e.message : String(e))

/** 每群覆寫：agent、模型、權限模式、YOLO（只有 owner 能改）。 */
export function RoomRoleDialog({ roomId, role, onClose, onChanged }: { roomId: number; role: string; onClose: () => void; onChanged: () => void }) {
  const [v, setV] = useState<ag.RoomOverrideView | null>(null)
  const [cat, setCat] = useState<ag.Catalog | null>(null)
  const [agent, setAgent] = useState('')
  const [model, setModel] = useState('')
  const [mode, setMode] = useState('')
  const [effort, setEffort] = useState('')
  const [account, setAccount] = useState('')
  const [accts, setAccts] = useState<ag.LoginAccount[]>([])
  const [yolo, setYolo] = useState(false)
  const [until, setUntil] = useState('')
  const [test, setTest] = useState(false)
  const [opts, setOpts] = useState<ag.Models | null>(null)
  const [busy, setBusy] = useState('')
  const [err, setErr] = useState('')

  useEffect(() => {
    void ag.roomOverride(roomId, role).then(x => {
      setV(x)
      setAgent(x.override.acp_agent ?? '')
      setModel(x.override.model ?? '')
      setMode(x.override.mode ?? '')
      setEffort(x.override.effort ?? '')
      setAccount(x.override.account ?? '')
      setYolo(!!x.override.yolo)
      setUntil(x.override.yolo_until ?? '')
      if (x.can_edit) void ag.catalog().then(setCat).catch(() => undefined)
      if (x.can_edit) void ag.accounts(true).then(a => setAccts(a.accounts)).catch(() => undefined)
    }).catch(e => setErr(msg(e)))
  }, [roomId, role])
  const effAgent = agent || v?.global.agent || ''
  useEffect(() => {
    if (!v?.can_edit || !effAgent) return
    setOpts(null)
    void ag.models(effAgent).then(setOpts).catch(e => setOpts({ default: null, models: [], source: msg(e) }))
  }, [v?.can_edit, effAgent])

  if (!v) return <Modal title={role} onClose={onClose}>{err ? <p className="err">{err}</p> : <p className="muted">{tr('Loading…')}</p>}</Modal>
  const sameAgent = !agent || agent === v.global.agent
  const inherit = (x: string | null | undefined) => x ? tr('Follow global: {x}', { x }) : tr('Follow global: default')
  const save = async () => {
    setErr(''); setBusy(model ? (test ? tr('Checking the model with a one-line test…') : tr('Checking the connection…')) : tr('Saving…'))
    try {
      await ag.setRoomOverride(roomId, role, { agent, model, mode, effort, account, yolo, yolo_until: yolo ? until : '', test })
      onChanged(); onClose()
    } catch (e) { setErr(msg(e)) } finally { setBusy('') }
  }
  const reset = async () => {
    setErr('')
    try { await ag.resetRoomOverride(roomId, role); onChanged(); onClose() } catch (e) { setErr(msg(e)) }
  }
  const hasOverride = Object.keys(v.override ?? {}).length > 0
  return (
    <Modal title={tr('{role} in this room', { role })} onClose={onClose}>
      <p className="muted small">{tr('Only this room changes. Other rooms keep the global settings from the Agents page.')}</p>
      <div className="kv">
        <span className="muted">{tr('Now')}</span>
        <span>{v.effective.agent ?? '?'} · {v.effective.model ?? tr('default model')}{v.effective.effort ? ` · ${v.effective.effort}` : ''}{v.effective.account ? ` · ${v.effective.account}` : ''} · {v.yolo ? <span className="tag warn">YOLO</span> : (v.effective.mode ?? tr('default permissions'))}</span>
      </div>
      {!v.can_edit ? <p className="muted small">{tr('Only the owner can change these settings.')}</p> : (
        <div className="edit">
          <label>{tr('Agent')}<select aria-label="room agent" value={agent} onChange={e => { setAgent(e.target.value); setModel(''); setMode(''); setEffort(''); setAccount('') }}>
            <option value="">{inherit(v.global.agent)}</option>
            {cat?.agents.filter(a => a.installed && a.id !== v.global.agent).map(a => <option key={a.id} value={a.id}>{a.label}</option>)}
          </select></label>
          <label>{tr('Model')}<select aria-label="room model" value={model} onChange={e => setModel(e.target.value)}>
            <option value="">{sameAgent ? inherit(v.global.model) : tr('Agent default')}</option>
            {opts?.models.map(m => <option key={m.id} value={m.id}>{m.name}</option>)}
            {model && !opts?.models.some(m => m.id === model) && <option value={model}>{model}</option>}
          </select>{!opts && <span className="muted small">{tr('loading models…')}</span>}</label>
          <label>{tr('Permissions')}<select aria-label="room mode" value={mode} disabled={yolo} onChange={e => setMode(e.target.value)}>
            <option value="">{sameAgent ? inherit(v.global.mode) : tr('Agent default')}</option>
            {opts?.modes?.map(m => <option key={m.id} value={m.id} title={m.description ?? ''}>{m.name}</option>)}
          </select>{opts && !opts.modes?.length && <span className="muted small">{tr('This agent reports no permission modes.')}</span>}</label>
          {['claude', 'codex'].includes(effAgent) && <label>{tr('Account')}<AccountPick agent={effAgent} value={account} onChange={setAccount} list={accts} allowDefault inherit={sameAgent && v.global.account ? tr('Follow global: {x}', { x: v.global.account }) : tr('Follow global: default')} /></label>}
          <label>{tr('Reasoning effort')}<EffortPick models={opts} value={effort} onChange={setEffort} inherit={sameAgent ? inherit(v.global.effort) : undefined} /></label>
          <div className={`yolo ${yolo ? 'on' : ''}`}>
            <label className="inline"><input type="checkbox" aria-label="yolo" checked={yolo} onChange={e => setYolo(e.target.checked)} /> <strong>YOLO</strong></label>
            <p className="small muted">{tr('The agent switches to its widest permission mode ({mode}) and every permission request in this room is allowed automatically. Reject options are never chosen.', { mode: opts?.widest_mode ?? tr('none — approvals only') })}</p>
            {yolo && <label className="inline small">{tr('Until (inclusive, empty = no end date)')} <input type="date" aria-label="yolo until" value={until} onChange={e => setUntil(e.target.value)} /></label>}
          </div>
          <label className="inline small"><input type="checkbox" aria-label="test model" checked={test} onChange={e => setTest(e.target.checked)} /> {tr('Also send a one-line test (a tiny amount of tokens)')}</label>
          <div className="row">
            <button className="primary" disabled={!!busy} onClick={() => void save()}>{tr('Save')}</button>
            {hasOverride && <button className="ghost" onClick={() => void reset()}>{tr('Reset to global')}</button>}
          </div>
          {busy && <p className="muted small">{busy}</p>}
        </div>
      )}
      {err && <p role="alert" className="err small">{err}</p>}
    </Modal>
  )
}
