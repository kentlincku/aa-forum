import { t as tr } from '../i18n'
import { Modal } from '../ui/bits'
import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react'
import type { User } from '../api'
import * as ag from './api'
import * as sk from '../skills/api'
import * as chat from '../chat/api'

const ago = (t: number) => {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - t))
  return s < 60 ? tr('{n}s ago', { n: s }) : s < 3600 ? tr('{n}m ago', { n: Math.floor(s / 60) }) : s < 86400 ? tr('{n}h ago', { n: Math.floor(s / 3600) }) : tr('{n}d ago', { n: Math.floor(s / 86400) })
}
const msg = (e: unknown) => (e instanceof Error ? e.message : String(e))

/** Agents 頁（Owner）：角色、加入 agent（安裝→探測→建立）、已安裝的 agent、額度。 */
export function AgentsPage({ user }: { user: User }) {
  const [cat, setCat] = useState<ag.Catalog | null>(null)
  const [health, setHealth] = useState<ag.RoleHealth[]>([])
  const [err, setErr] = useState('')
  const [adding, setAdding] = useState(false)
  const load = useCallback(async () => {
    try {
      const [c, h] = await Promise.all([ag.catalog(), ag.health()])
      setCat(c); setHealth(h.roles); setErr('')
    } catch (e) { setErr(msg(e)) }
  }, [])
  useEffect(() => { void load() }, [load])
  if (user.rank !== 'owner') return <main className="page"><h2>{tr("Agents")}</h2><p className="muted">{tr("Agent management is owner-only.")}</p></main>
  return (
    <main className="page agents">
      <div className="row page-head"><h2>{tr("Agents")}</h2><span className="actions"><button onClick={() => void load()}>{tr("Refresh")}</button><button className="primary" onClick={() => setAdding(true)}>{tr("＋ Add agent")}</button></span></div>
      {err && <p role="alert" className="err">{err}</p>}
      {adding && cat && <AddAgent cat={cat} onClose={() => setAdding(false)} onDone={() => { setAdding(false); void load() }} onChanged={load} />}
      <section aria-label="roles">
        <h3>{tr("Roles")}</h3>
        {health.length === 0 && <p className="muted">{tr("No roles yet. Add an agent to create one.")}</p>}
        <div className="cards">
          {health.map(r => <RoleCard key={`${r.role}:${r.agent}:${r.model}:${r.rank}:${r.mode}:${r.transport}:${r.effort}:${r.account}:${JSON.stringify(r.room_overrides)}`} r={r} cat={cat} onChanged={load} />)}
        </div>
      </section>
      {cat && <Programs cat={cat} onChanged={load} />}
      <AccountsSection />
      <TeamSettings />
    </main>
  )
}

function RoleCard({ r, cat, onChanged }: { r: ag.RoleHealth; cat: ag.Catalog | null; onChanged: () => void }) {
  const [edit, setEdit] = useState(false)
  const [agent, setAgent] = useState(r.agent ?? '')
  const [model, setModel] = useState(r.model ?? '')
  const [rank, setRank] = useState(r.rank)
  const [mode, setMode] = useState(r.mode ?? '')
  const [effort, setEffort] = useState(r.effort ?? '')
  const [account, setAccount] = useState(r.account ?? '')
  const [accts, setAccts] = useState<ag.LoginAccount[]>([])
  const [test, setTest] = useState(false)
  const [transport, setTransport] = useState<'acp' | 'headless'>(r.transport === 'headless' ? 'headless' : 'acp')
  const canHeadless = ['claude', 'codex', 'pi'].includes(agent)
  const canAccounts = ['claude', 'codex'].includes(agent)
  useEffect(() => { if (edit && canAccounts) void ag.accounts(true).then(x => setAccts(x.accounts)).catch(() => undefined) }, [edit, canAccounts])
  const [models, setModels] = useState<ag.Models | null>(null)
  const [busy, setBusy] = useState('')
  const [err, setErr] = useState('')
  useEffect(() => {
    if (!edit || !agent) return
    setModels(null)
    void ag.models(agent).then(setModels).catch(e => setModels({ default: null, models: [], source: msg(e) }))
  }, [edit, agent])
  const save = async () => {
    setErr(''); setBusy(model && model !== r.model ? (test ? tr('Checking the model with a one-line test…') : tr('Checking the connection…')) : tr('Saving…'))
    try {
      await ag.setEngine(r.role, { agent: agent !== r.agent ? agent : undefined, model: model !== (r.model ?? '') ? model : undefined, rank: rank !== r.rank ? rank : undefined, mode: mode !== (r.mode ?? '') ? mode : undefined, effort: effort !== (r.effort ?? '') ? effort : undefined, account: account !== (r.account ?? '') ? account : undefined, test, transport: canHeadless && transport !== (r.transport === 'headless' ? 'headless' : 'acp') ? transport : undefined })
      setEdit(false); onChanged()
    } catch (e) { setErr(msg(e)) } finally { setBusy('') }
  }
  const remove = async () => {
    if (!confirm(tr('Remove role {role}? Its current turn finishes first; rooms keep their history.', { role: r.role }))) return
    try { await ag.removeRole(r.role); onChanged() } catch (e) { setErr(msg(e)) }
  }
  const working = r.lanes.filter(l => l.status === 'working').length
  const lt = r.last_turn
  const roomErr = r.room_overrides?.find(o => o.agent_error)
  const state = r.agent_error || roomErr || (lt && lt.outcome !== 'ok' && lt.outcome !== 'running') ? 'error' : working ? 'working' : r.lanes.length ? 'idle' : 'offline'
  const stateLabel = { error: tr('error'), working: tr('working'), idle: tr('idle'), offline: tr('offline') }[state]
  return (
    <>
      <button className="card role-card" aria-label={`role ${r.role}`} onClick={() => setEdit(true)}>
        <div className="row"><span><strong>{r.role}</strong>{r.label && <span className="muted"> {r.label}</span>}</span><span className={`tag ${state}`}>{stateLabel}</span></div>
        <div className="muted small">{r.model ?? tr("default model")} · {r.agent ?? (r.driver === 'acp' ? tr('custom command') : r.driver)}{r.effort ? ` · ${r.effort}` : ''}{r.account ? ` · ${r.account}` : ''}{r.transport === 'headless' ? ' · headless' : ''}{r.rank === 'lead' ? ' · lead' : ''}</div>
        {(r.room_overrides?.length ?? 0) > 0 && <div className="row wrap small"><span className="tag">{tr('{n} rooms customised', { n: r.room_overrides!.length })}</span>{r.room_overrides!.some(o => o.yolo) && <span className="tag warn">YOLO</span>}</div>}
        {r.agent_error && <div className="err small">{r.agent_error}</div>}
        {!r.agent_error && roomErr && <div className="err small">{tr('Room {room}:', { room: roomErr.room })} {roomErr.agent_error}</div>}
        {lt && <div className="muted small">{tr("· last turn").replace(/^·\s*/, '')} {ago(lt.started_at)}</div>}
      </button>
      {edit && (
        <Modal title={r.label ? `${r.role} ${r.label}` : r.role} onClose={() => setEdit(false)}>
          <div className="muted small">{tr('{n} sessions', { n: r.lanes.length })}{working ? tr(' · {n} working', { n: working }) : ''}
            {lt && <> {tr("· last turn")}{' '}{ago(lt.started_at)} — <span className={lt.outcome === 'ok' ? '' : 'err'}>{tr(lt.outcome)}</span></>}</div>
          {lt?.error && lt.outcome !== 'ok' && <div className="err small" title={lt.error}>{lt.error.slice(0, 300)}</div>}
          {r.agent_error && <div role="alert" className="err small">{r.agent_error}</div>}
          <div className="edit">
            <label>{tr("Agent")}{' '}<select value={agent} onChange={e => { setAgent(e.target.value); setModel(''); setEffort(''); setAccount('') }}>
              {r.agent_error && r.agent && <option value={r.agent} disabled>{r.agent}</option>}
              {cat?.agents.filter(a => a.installed).map(a => <option key={a.id} value={a.id}>{a.label}</option>)}
            </select></label>
            <label>{tr("Model")}{' '}<ModelPick models={models} value={model} onChange={setModel} /></label>
            <label>{tr('Permissions')}{' '}<select aria-label="global mode" value={mode} onChange={e => setMode(e.target.value)}>
              <option value="">{tr('Agent default')}</option>
              {models?.modes?.map(m => <option key={m.id} value={m.id} title={m.description ?? ''}>{m.name}</option>)}
            </select><span className="muted small"> {tr('YOLO can only be turned on per room.')}</span></label>
            <label>{tr('Reasoning effort')}{' '}<EffortPick models={models} value={effort} onChange={setEffort} /></label>
            {canAccounts && <label>{tr('Account')}{' '}<AccountPick agent={agent} value={account} onChange={setAccount} list={accts} /></label>}
            {canHeadless && <label>{tr('Connection')}{' '}<select aria-label="transport" value={transport} onChange={e => setTransport(e.target.value as 'acp' | 'headless')}>
              <option value="acp">ACP</option>
              <option value="headless">{tr('Headless CLI')}</option>
            </select>{transport === 'headless' && <span className="muted small"> {tr("Runs the agent's own CLI each turn. No permission cards: YOLO rooms allow everything, other rooms use a fixed safe policy.")}</span>}</label>}
            <label>{tr("Rank")}{' '}<select value={rank} onChange={e => setRank(e.target.value)}><option value="worker">{tr("worker")}</option><option value="lead">{tr("lead")}</option></select></label>
            <label className="inline small"><input type="checkbox" aria-label="test model" checked={test} onChange={e => setTest(e.target.checked)} /> {tr('Also send a one-line test (a tiny amount of tokens)')}</label>
            <p className="muted small">{tr("Changing the model first checks that the agent connects and offers that model (no tokens); nothing changes if it fails. The role finishes its current turn, then starts a fresh session.")}</p>
            <div className="row"><button className="primary" disabled={!!busy} onClick={() => void save()}>{tr("Save")}</button><button className="ghost danger" onClick={() => void remove()}>{tr("Remove")}</button></div>
            {busy && <p className="muted">{busy}</p>}
          </div>
          {err && <p role="alert" className="err small">{err}</p>}
        </Modal>
      )}
    </>
  )
}

function ModelPick({ models, value, onChange }: { models: ag.Models | null; value: string; onChange: (m: string) => void }) {
  if (!models) return <span className="muted small">{tr("loading models…")}</span>
  const dflt = models.default ? `${tr('Agent default')} (${models.default})` : tr('Agent default')
  return (
    <span>
      {models.models.length
        ? <select aria-label="model" value={value} onChange={e => onChange(e.target.value)}>
            <option value="">{dflt}</option>
            {models.models.map(m => <option key={m.id} value={m.id}>{m.name}</option>)}
            {value && !models.models.some(m => m.id === value) && <option value={value}>{value}</option>}
          </select>
        : <input aria-label="model" value={value} placeholder={dflt} onChange={e => onChange(e.target.value)} />}
      <span className="muted small"> {models.models.length} {tr("listed ·")}{' '}{models.source}</span>
    </span>
  )
}

/** 加入 agent：選 agent →（需要時）安裝並看即時輸出 → 探測 → 建立角色。 */
function AddAgent({ cat, onClose, onDone, onChanged }: { cat: ag.Catalog; onClose: () => void; onDone: () => void; onChanged: () => void }) {
  const [agent, setAgent] = useState('')
  const [custom, setCustom] = useState('')
  const [plan, setPlan] = useState<ag.Plan | null>(null)
  const [job, setJob] = useState<ag.Job | null>(null)
  const [out, setOut] = useState('')
  const [probe, setProbe] = useState<ag.Probe | null>(null)
  const [role, setRole] = useState('')
  const [label, setLabel] = useState('')
  const [rank, setRank] = useState('worker')
  const [model, setModel] = useState('')
  const [skills, setSkills] = useState<sk.SkillInfo[]>([])
  const [picked, setPicked] = useState<string[]>([])
  const [err, setErr] = useState('')
  const [busy, setBusy] = useState(false)
  const outRef = useRef<HTMLPreElement>(null)
  const a = cat.agents.find(x => x.id === agent)
  useEffect(() => { void sk.list().then(l => setSkills(l.skills)).catch(() => undefined) }, [])
  useEffect(() => { outRef.current?.scrollTo(0, outRef.current.scrollHeight) }, [out])
  // 安裝進度輪詢
  useEffect(() => {
    if (!job || job.state !== 'running') return
    const iv = setInterval(() => {
      void ag.job(job.id, job.offset).then(j => {
        setOut(o => o + j.output); setJob(j)
        if (j.state !== 'running') onChanged()
      }).catch(e => setErr(msg(e)))
    }, 700)
    return () => clearInterval(iv)
  }, [job, onChanged])
  const runProbe = async (id: string) => {
    setProbe(null); setErr(''); setBusy(true)
    try { setProbe(await ag.probe(id)) } catch (e) { setErr(msg(e)) } finally { setBusy(false) }
  }
  const pick = (id: string) => {
    setAgent(id); setPlan(null); setJob(null); setOut(''); setProbe(null); setErr(''); setModel('')
    const x = cat.agents.find(y => y.id === id)
    if (x?.installed) void runProbe(id)
  }
  const doPlan = async (e: FormEvent) => {
    e.preventDefault(); setErr(''); setPlan(null)
    try { setPlan(await ag.planCustom(custom.trim())) } catch (x) { setErr(msg(x)) }
  }
  const doInstall = async (b: { agent?: string; npm?: string }, what: string) => {
    if (!confirm(tr('Install {what} into the AA Forum vendor folder?', { what }))) return
    setErr(''); setOut('')
    try { const j = await ag.install(b); setJob({ ...j, output: '', offset: 0 }) } catch (x) { setErr(msg(x)) }
  }
  const installedId = job?.state === 'done' ? job.agent : a?.installed ? agent : ''
  useEffect(() => { if (job?.state === 'done') { setAgent(job.agent); void runProbe(job.agent) } }, [job?.state]) // eslint-disable-line react-hooks/exhaustive-deps
  const create = async (e: FormEvent) => {
    e.preventDefault(); setErr(''); setBusy(true)
    try {
      await ag.addRole({ role: role.trim(), agent: installedId, model: model.trim() || undefined, rank, label: label.trim() || undefined, skills: picked })
      onDone()
    } catch (x) { setErr(msg(x)) } finally { setBusy(false) }
  }
  return (
    <div className="card add-agent" role="dialog" aria-label="add agent">
      <div className="row"><h3>{tr("Add agent")}</h3><button onClick={onClose} aria-label="close">✕</button></div>
      <ol className="steps">
        <li>
          <strong>{tr("Choose an agent")}</strong>
          <div className="row wrap">
            {cat.agents.map(x => (
              <button key={x.id} className={agent === x.id ? 'active' : ''} onClick={() => pick(x.id)}>
                {x.label} {x.installed ? '✓' : ''}
              </button>
            ))}
            <button className={agent === '_custom' ? 'active' : ''} onClick={() => pick('_custom')}>{tr("Custom npm…")}</button>
          </div>
          {a && <p className="muted small">{a.needs}{a.installed && a.logged_in === false && <span className="err"> {tr("— not logged in")}</span>}</p>}
        </li>
        {agent === '_custom' && !job && (
          <li>
            <form className="row" onSubmit={doPlan}>
              <input aria-label="npm package" placeholder={tr("some-acp or @scope/pkg@1.2.3")} value={custom} onChange={e => setCustom(e.target.value)} />
              <button type="submit">{tr("Look up")}</button>
            </form>
            {plan && (
              <div className="small">
                <p>{plan.label} {plan.npm} {tr("— runs")}{' '}<code>{plan.acp}</code> {tr("(bins:")}{' '}{plan.bins.join(', ')})</p>
                <pre className="mono small">{plan.install_cmd.join(' ')}</pre>
                <button onClick={() => void doInstall({ npm: plan.npm }, plan.npm)}>{tr("Install")}</button>
              </div>
            )}
          </li>
        )}
        {a && !a.installed && !job && (
          <li>
            {a.npm ? <>
              <p className="small">{tr("Not installed. Will run")}{' '}<code>{tr("npm install")}{' '}{a.npm}</code> {tr("in the vendor folder.")}</p>
              <button onClick={() => void doInstall({ agent: a.id }, a.npm!)}>{tr("Install")}{' '}{a.label}</button>
            </> : <p className="err small">{tr("Install this yourself:")}{' '}{a.needs}</p>}
          </li>
        )}
        {job && (
          <li>
            <strong>{tr("Installing")}{' '}{job.npm}</strong> <span className={job.state === 'failed' ? 'err' : 'muted'}>{job.state}</span>
            <pre ref={outRef} className="log" aria-label="install output">{out}</pre>
          </li>
        )}
        {installedId && (
          <li>
            <strong>{tr("Connection check")}</strong>{' '}
            {busy && !probe ? <span className="muted">{tr("starting the agent (no prompt is sent)…")}</span>
              : probe?.ok ? <span>{tr("✓ connected · model")}{' '}{probe.model ?? 'not reported'} · {probe.models?.length ?? 0} {tr("models")}</span>
              : probe ? <span className="err">{tr("failed:")}{' '}{probe.error}</span> : null}
            {probe && !probe.ok && <button onClick={() => void runProbe(installedId)}>{tr("Retry")}</button>}
          </li>
        )}
        {installedId && probe?.ok && (
          <li>
            <form className="grid-form" onSubmit={create}>
              <label>{tr("Role name")}{' '}<input aria-label="role name" required pattern="[a-z0-9_-]{1,32}" title="a-z 0-9 - _" value={role} onChange={e => setRole(e.target.value)} /></label>
              <label>{tr("Display name")}{' '}<input aria-label="display name" value={label} onChange={e => setLabel(e.target.value)} /></label>
              <label>{tr("Rank")}{' '}<select aria-label="rank" value={rank} onChange={e => setRank(e.target.value)}><option value="worker">{tr("worker")}</option><option value="lead">{tr("lead (can coordinate tasks)")}</option></select></label>
              <label>{tr("Model")}{' '}<ModelPick models={{ default: probe.model ?? null, models: probe.models ?? [], source: 'from the agent' }} value={model} onChange={setModel} /></label>
              {skills.length > 0 && (
                <fieldset><legend>{tr("Skills")}</legend>
                  {skills.map(s => <label key={s.id} className="inline"><input type="checkbox" checked={picked.includes(s.id)} onChange={e => setPicked(p => e.target.checked ? [...p, s.id] : p.filter(x => x !== s.id))} /> {s.id}</label>)}
                </fieldset>
              )}
              <button type="submit" disabled={busy}>{tr("Create role")}</button>
            </form>
          </li>
        )}
      </ol>
      {err && <p role="alert" className="err">{err}</p>}
    </div>
  )
}

/** Agent 程式（一張卡一個 agent）：安裝／登入、套件版本與升級、帳號額度。 */
function Programs({ cat, onChanged }: { cat: ag.Catalog; onChanged: () => void }) {
  const [pkgs, setPkgs] = useState<ag.Package[]>([])
  const [quota, setQuota] = useState<ag.Account[] | null>(null)
  const [err, setErr] = useState('')
  const [checking, setChecking] = useState(false)
  const [job, setJob] = useState<ag.Job | null>(null)
  const [out, setOut] = useState('')
  const outRef = useRef<HTMLPreElement>(null)
  const loadPkgs = useCallback(() => { void ag.packages().then(r => setPkgs(r.packages)).catch(e => setErr(msg(e))) }, [])
  const loadQuota = useCallback(() => { void ag.quota().then(r => setQuota(r.accounts)).catch(() => setQuota([])) }, [])
  useEffect(() => { loadPkgs(); loadQuota() }, [loadPkgs, loadQuota])
  useEffect(() => { outRef.current?.scrollTo(0, outRef.current.scrollHeight) }, [out])
  useEffect(() => {
    if (!job || job.state !== 'running') return
    const iv = setInterval(() => {
      void ag.job(job.id, job.offset).then(j => {
        setOut(o => o + j.output); setJob(j)
        if (j.state !== 'running') { loadPkgs(); onChanged() }
      }).catch(e => setErr(msg(e)))
    }, 700)
    return () => clearInterval(iv)
  }, [job, loadPkgs, onChanged])
  const check = async () => {
    setErr(''); setChecking(true)
    try { setPkgs((await ag.checkPackages()).packages) } catch (e) { setErr(msg(e)) } finally { setChecking(false) }
  }
  const run = async (p: ag.Package, version: string) => {
    const roles = p.used_by.length ? tr(' Roles using it ({roles}) finish their current turn, then restart with the new version.', { roles: p.used_by.join(', ') }) : ''
    if (!confirm(tr('Install {pkg}@{v}? It is tested in a staging folder first; if the test fails nothing changes.', { pkg: p.npm ?? p.agent, v: version }) + roles)) return
    setErr(''); setOut('')
    try { const j = await ag.upgrade(p.agent, version); setJob({ ...j, output: '', offset: 0 }) } catch (e) { setErr(msg(e)) }
  }
  const runEngine = async (p: ag.Package, version: string) => {
    const roles = p.used_by.length ? tr(' Roles using it ({roles}) finish their current turn, then restart with the new version.', { roles: p.used_by.join(', ') }) : ''
    if (!confirm(tr('Update {agent} itself to {v}? The new version is tested with the current adapter first; your installed copy is only replaced if the test passes.', { agent: p.label, v: version }) + roles)) return
    setErr(''); setOut('')
    try { const j = await ag.engineUpgrade(p.agent, version); setJob({ ...j, output: '', offset: 0 }) } catch (e) { setErr(msg(e)) }
  }
  const [testing, setTesting] = useState('')
  const test = async (p: ag.Package) => {
    setErr(''); setTesting(p.agent)
    try { const r = await ag.testCompat(p.agent); if (!r.ok) setErr(`${p.label}: ${r.error ?? ''}`) } catch (e) { setErr(msg(e)) } finally { setTesting(''); loadPkgs() }
  }
  const busy = job?.state === 'running' || !!testing
  const checked = pkgs.some(p => p.checked_at)
  return (
    <section aria-label="agent programs">
      <div className="row page-head"><h3>{tr('Agent programs')}</h3>
        <span className="actions">
          {checked && <span className="muted small">{tr('checked {t}', { t: ago(Math.max(...pkgs.map(p => p.checked_at ?? 0))) })}</span>}
          <button disabled={checking || busy} onClick={() => void check()}>{checking ? tr('Checking…') : tr('Check for updates')}</button>
          <button onClick={loadQuota}>{tr('Refresh quota')}</button>
        </span></div>
      {err && <p role="alert" className="err">{err}</p>}
      <table className="tbl programs">
        <thead><tr>
          <th>{tr('Agent')}</th><th>{tr('ACP adapter')}</th><th>{tr('Agent engine')}</th><th>{tr('Compatibility')}</th><th>{tr('Quota')}</th><th />
        </tr></thead>
        <tbody>
          {cat.agents.map(a => {
            const p = pkgs.find(x => x.agent === a.id)
            const q = quota?.find(x => x.agent === a.id)
            return <ProgramRow key={a.id} a={a} p={p} q={q} quotaLoading={quota === null} busy={busy} testing={testing === a.id} run={run} runEngine={runEngine} test={test} />
          })}
        </tbody>
      </table>
      {job && (
        <div className="card job" aria-label="upgrade job">
          <div className="row"><strong>{job.npm}</strong><span className={`tag ${job.state === 'failed' ? 'error' : job.state === 'done' ? 'ok' : 'working'}`}>{tr(job.state)}</span></div>
          <pre ref={outRef} className="log" aria-label="upgrade output">{out}</pre>
        </div>
      )}
      <p className="muted small">{tr("Adapters install into")}{' '}<span className="mono">{cat.vendor}</span></p>
    </section>
  )
}

function ProgramRow({ a, p, q, quotaLoading, busy, testing, run, runEngine, test }: { a: ag.AgentStatus; p?: ag.Package; q?: ag.Account; quotaLoading: boolean; busy: boolean
  testing: boolean; run: (p: ag.Package, v: string) => void; runEngine: (p: ag.Package, v: string) => void; test: (p: ag.Package) => void }) {
  const adapterIn = a.installed || !!p?.installed
  const en = p?.engine
  const engNew = en?.managed && en.version && en.latest && en.version !== en.latest
  const cs = p?.compat?.state
  const state = !adapterIn ? (a.cli_installed ? 'adapter-missing' : 'missing') : a.logged_in === false ? 'logged-out' : 'ok'
  const label = { ok: tr('ready'), 'logged-out': tr('not logged in'), 'adapter-missing': tr('adapter missing'), missing: tr('not installed') }[state]
  const outdated = p?.installed && p.latest && p.installed !== p.latest && p.latest !== p.pinned
  // 沒裝（含只裝了改名前的舊套件）或版本不是建議版本：提供安裝／改用建議版本
  const offPin = !!p?.npm && !!p.pinned && p.installed !== p.pinned
  const notes: { warn?: boolean; text: string; title?: string }[] = []
  p?.legacy.forEach(l => notes.push({ warn: true, text: tr('old package {pkg} {v} still installed', { pkg: l.npm, v: l.version }) }))
  if (p?.deprecated) notes.push({ warn: true, text: tr('deprecated on npm'), title: p.deprecated })
  if (p?.check_error) notes.push({ warn: true, text: tr('could not check'), title: p.check_error })
  if (state === 'logged-out' || state === 'missing' || state === 'adapter-missing') notes.push({ text: a.needs })
  if (cs === 'fail') notes.push({ warn: true, text: tr('The last test with these versions failed; roles keep running, but may not work.'), title: p?.compat?.error ?? '' })
  if (cs === 'unknown' && p?.compat?.last) notes.push({ warn: true, text: tr('Versions changed since the last test.') })
  const ver = (cur: string | null | undefined, rec?: string | null, latest?: string | null, isNew?: boolean) => (
    <span className="vers"><b>{cur ?? '—'}</b>
      {rec && rec !== cur && <span className="muted small"> {tr('recommended {v}', { v: rec })}</span>}
      {latest && latest !== cur && latest !== rec && <span className={`small ${isNew ? 'new' : 'muted'}`}> {tr('latest {v}', { v: latest })}</span>}
    </span>)
  return (
    <>
      <tr className="prog" aria-label={`agent ${a.id}`}>
        <td><strong>{a.label}</strong>{a.custom && <span className="muted small"> {tr("(custom)")}</span>}
          <div><span className={`tag ${state === 'ok' ? 'ok' : state === 'missing' ? '' : 'error'}`}>{label}</span></div></td>
        <td>{p?.npm ? <>{ver(p.installed, p.pinned, p.check_error ? null : p.latest, !!outdated)}<div className="mono small muted">{p.npm}</div></>
          : <span className="muted small">{a.command?.[0]?.split('/').pop() ?? '—'}</span>}</td>
        <td>{en ? <>{ver(en.version, null, en.managed ? en.latest : null, !!engNew)}
          <div className="muted small">{en.source === 'bundled' ? tr('bundled in the adapter') : tr('your CLI')}</div></> : '—'}</td>
        <td>{en ? <span className={`compat ${cs ?? ''}`} title={p?.compat?.error ?? (p?.compat?.at ? tr('tested {t}', { t: ago(p.compat.at) }) : '')}>
          {cs === 'ok' ? tr('✓ tested') : cs === 'fail' ? tr('✗ failed') : tr('not tested')}</span> : '—'}
          {p?.compat?.at && <div className="muted small">{ago(p.compat.at)}</div>}</td>
        <td className="q">{adapterIn ? <Usage q={q} loading={quotaLoading} agent={a.id} account="default" note={q?.note} /> : null}</td>
        <td className="acts">
          {p && offPin && <button className="primary" disabled={busy} onClick={() => run(p, p.pinned!)}>{p.installed ? tr('Use {v}', { v: p.pinned! }) : tr('Install')}</button>}
          {p && outdated && <button className="primary" disabled={busy} onClick={() => run(p, p.latest!)}>{tr('Upgrade to {v}', { v: p.latest! })}</button>}
          {p && adapterIn && engNew && <button className="primary" disabled={busy} onClick={() => runEngine(p, en!.latest!)}>{tr('Update {agent} to {v}', { agent: a.label, v: en!.latest! })}</button>}
          {p && adapterIn && en && <button className="ghost" disabled={busy} onClick={() => test(p)}>{testing ? tr('Testing…') : tr('Test compatibility')}</button>}
        </td>
      </tr>
      {(notes.length > 0 || (p?.used_by.length ?? 0) > 0) && (
        <tr className="prog-notes"><td />
          <td colSpan={5} className="small">
            {p?.used_by.length ? <span className="muted">{tr('used by {roles}', { roles: p.used_by.join(', ') })}</span> : null}
            {notes.map((n, i) => <span key={i} className={n.warn ? 'warn' : 'muted'} title={n.title}>{n.text}</span>)}
          </td></tr>
      )}
    </>
  )
}

function Usage({ q, loading, agent, account, note }: { q?: { windows: ag.Window[]; error?: string; observed_at?: number; from_turn?: boolean }; loading: boolean; agent?: string; account?: string; note?: string }) {
  const [local, setLocal] = useState<{ windows: ag.Window[]; at: number } | null>(null)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState('')
  if (loading) return <div className="muted small">{tr("Loading…")}</div>
  const canCheck = agent === 'claude'
  const windows = local?.windows ?? q?.windows ?? []
  if (!canCheck && (!q || (windows.length === 0 && !q.error))) return null
  const observed = local?.at ?? (q?.from_turn ? q.observed_at : undefined)
  const check = () => {
    setBusy(true); setErr('')
    void ag.refreshClaudeQuota(account).then(r => setLocal({ windows: r.windows, at: r.observed_at })).catch(e => setErr(msg(e))).finally(() => setBusy(false))
  }
  return (
    <div className="usage">
      {windows.map(w => (
        <div key={w.label} className="quota-row">
          <span className="ql" title={w.count ?? ''}>{tr(w.label)}</span>
          <div className="bar"><div style={{ width: `${Math.max(0, Math.min(100, w.remaining_percent))}%` }} className={w.remaining_percent < 15 ? 'low' : w.remaining_percent < 40 ? 'mid' : ''} /></div>
          <span className="qv">{w.unlimited ? tr('unlimited') : `${w.remaining_percent}%`}</span>
          {w.resets_at && <span className="muted small qr">{tr("resets")}{' '}{new Date(w.resets_at * 1000).toLocaleDateString(undefined, { month: 'numeric', day: 'numeric' })}</span>}
        </div>
      ))}
      {windows.length === 0 && note && <div className="muted small">{note}</div>}
      {q?.error && <p className="err small">{tr("Could not read:")}{' '}{q.error}</p>}
      {canCheck && (
        <div className="row small">
          {observed ? <span className="muted">{tr('from the last turn')}{' '}{new Date(observed * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span> : null}
          <button className="ghost small" disabled={busy} title={tr('Checks by sending a one-word turn (haiku): uses a tiny bit of quota')} onClick={check}>{busy ? tr('Checking…') : tr('Check now')}</button>
        </div>
      )}
      {err && <p className="err small">{err}</p>}
    </div>
  )
}

/** Dashboard：概況、警示、未讀、最近回合。 */
export function DashboardPage({ go }: { go: (p: string) => void }) {
  const [d, setD] = useState<ag.Dashboard | null>(null)
  const [err, setErr] = useState('')
  const load = useCallback(() => { void ag.dashboard().then(x => { setD(x); setErr('') }).catch(e => setErr(msg(e))) }, [])
  useEffect(() => { load(); const iv = setInterval(load, 10000); return () => clearInterval(iv) }, [load])
  if (err) return <main className="page"><h2>{tr("Dashboard")}</h2><p className="err">{err}</p></main>
  if (!d) return <main className="page"><p className="muted">{tr("Loading…")}</p></main>
  const tt = d.turns_24h
  const total = Object.values(tt).reduce((n, x) => n + x.count, 0)
  const failed = (tt.failed?.count ?? 0) + (tt.start_failed?.count ?? 0)
  return (
    <main className="page dashboard">
      <h2>{tr("Dashboard")}</h2>
      <div className="stats">
        <Stat label={tr("Roles")} value={d.roles} />
        <Stat label={tr("Sessions open")} value={d.lanes_open} />
        <Stat label={tr("Working now")} value={d.working} />
        <Stat label={tr("Rooms")} value={d.rooms} />
        <Stat label={tr("Turns (24 h)")} value={total} sub={tt.ok?.avg_secs != null ? tr('avg {n}s', { n: tt.ok.avg_secs }) : undefined} />
        <Stat label={tr("Failed (24 h)")} value={failed} warn={failed > 0} />
      </div>
      {d.alerts.length > 0 && (
        <section aria-label="alerts">
          <h3>{tr("Needs attention")}</h3>
          {d.alerts.map(a => (
            <div key={a.id} className="alert row">
              <strong>{a.role}</strong>
              <span>{tr(a.outcome === 'start_failed' ? 'could not start' : 'turn failed')}{a.room != null && <> {tr("in")}{' '}<a href={`#/rooms/${a.room}`}>{tr("room #")}{a.room}</a></>}</span>
              <span className="muted small">{ago(a.started_at)}</span>
              {a.error && <span className="err small" title={a.error}>{a.error.slice(0, 140)}</span>}
            </div>
          ))}
        </section>
      )}
      {d.queued.length > 0 && (
        <section aria-label="waiting">
          <h3>{tr("Waiting for agents")}</h3>
          {d.queued.map(q => <div key={q.room}><a href={`#/rooms/${q.room}`}>{q.name}</a> — {tr('{n} unread', { n: q.queued })}</div>)}
        </section>
      )}
      <section aria-label="recent turns">
        <h3>{tr("Recent turns")}</h3>
        {d.recent.length === 0 && <p className="muted">{tr("No turns yet.")}</p>}
        <table className="tbl">
          <tbody>
            {d.recent.map(r => (
              <tr key={r.id} className="clickable" onClick={() => r.room != null && go(`/rooms/${r.room}`)}>
                <td>{r.role}</td>
                <td>{r.room != null ? tr("room #{n}", { n: r.room }) : "—"}</td>
                <td className={r.outcome === 'ok' ? '' : r.outcome === 'running' ? 'muted' : 'err'}>{tr(r.outcome)}</td>
                <td>{r.ended_at ? `${r.ended_at - r.started_at}s` : ''}</td>
                <td className="muted small">{ago(r.started_at)}</td>
                <td className="err small">{r.outcome !== 'ok' ? r.error?.slice(0, 80) : ''}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </main>
  )
}

function Stat({ label, value, sub, warn }: { label: string; value: number; sub?: string; warn?: boolean }) {
  return <div className={`stat card${warn ? ' warn' : ''}`}><div className="v">{value}</div><div className="muted small">{label}{sub ? ` · ${sub}` : ''}</div></div>
}

/** 群聊行為設定：agent 互相叫醒的上限（頁面值優先於 .aaf.env，存了立刻生效）。 */
function TeamSettings() {
  const [s, setS] = useState<chat.ChatSettings | null>(null)
  const [value, setValue] = useState('')
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState('')
  const [saved, setSaved] = useState(false)
  const apply = (x: chat.ChatSettings) => { setS(x); setValue(String(x.agent_chain)) }
  useEffect(() => { void chat.settings().then(apply).catch(e => setErr(msg(e))) }, [])
  const save = async (v: number | null) => {
    setErr(''); setSaved(false); setBusy(true)
    try { apply(await chat.setSettings({ agent_chain: v })); setSaved(true) } catch (e) { setErr(msg(e)) } finally { setBusy(false) }
  }
  if (!s) return err ? <section aria-label="team settings"><h3>{tr('Team behavior')}</h3><p role="alert" className="err small">{err}</p></section> : null
  const n = Number(value)
  const valid = value.trim() !== '' && Number.isInteger(n) && n >= 0 && n <= s.agent_chain_max
  return (
    <section aria-label="team settings">
      <h3>{tr('Team behavior')}</h3>
      <div className="card">
        <label>{tr('Agent-only messages before agents stop waking each other')}{' '}
          <input type="number" aria-label="agent chain limit" min={0} max={s.agent_chain_max} step={1} value={value} onChange={e => { setValue(e.target.value); setSaved(false) }} style={{ width: '6em' }} />
        </label>
        <p className="muted small">{tr('Counted per room since the last human message. Past the limit, agent messages are still posted but wake nobody until someone posts. 0 = no limit. Takes effect immediately.')}</p>
        <p className="muted small">{s.agent_chain_source === 'page'
          ? tr('Set on this page (overrides the config file value {n}).', { n: s.agent_chain_config })
          : tr('From the config file (AAF_CHAT_AGENT_CHAIN, default 8).')}</p>
        <div className="row">
          <button className="primary" disabled={busy || !valid || n === s.agent_chain} onClick={() => void save(n)}>{tr('Save')}</button>
          {s.agent_chain_source === 'page' && <button className="ghost" disabled={busy} onClick={() => void save(null)}>{tr('Use config file value')}</button>}
          {saved && <span className="muted small">{tr('Saved')}</span>}
        </div>
        {err && <p role="alert" className="err small">{err}</p>}
      </div>
    </section>
  )
}

/** 推理強度選單：選項來自 agent 回報（預設模型的清單；其他模型存檔時由伺服器再驗）。 */
export function EffortPick({ models, value, onChange, inherit }: { models: ag.Models | null; value: string; onChange: (v: string) => void; inherit?: string }) {
  const list = models?.efforts ?? []
  if (models && !list.length && !value) return <span className="muted small">{tr('This agent reports no reasoning effort setting.')}</span>
  return (
    <select aria-label="reasoning effort" value={value} onChange={e => onChange(e.target.value)}>
      <option value="">{inherit ?? tr('Agent default')}</option>
      {list.map(x => <option key={x} value={x}>{x}</option>)}
      {value && !list.includes(value) && <option value={value}>{value}</option>}
    </select>
  )
}

/** 帳號選單：預設登入＋這個 agent 的帳號。light 清單只有 id，不查登入狀態。 */
export function AccountPick({ agent, value, onChange, list, inherit, allowDefault }: { agent: string; value: string; onChange: (v: string) => void; list: ag.LoginAccount[]; inherit?: string; allowDefault?: boolean }) {
  const mine = list.filter(a => a.agent === agent && !a.default)
  return (
    <select aria-label="account" value={value} onChange={e => onChange(e.target.value)}>
      <option value="">{inherit ?? tr('Default login')}</option>
      {allowDefault && <option value="default">{tr('Default login')}</option>}
      {mine.map(a => <option key={a.id} value={a.id}>{a.label ? `${a.label} (${a.id})` : a.id}</option>)}
      {value && value !== 'default' && !mine.some(a => a.id === value) && <option value={value}>{value}</option>}
    </select>
  )
}

/** 帳號：每個 Claude／Codex 帳號有自己的登入資料夾，角色或群可以選用。aaf 不碰憑證，登入在終端機做。 */
function AccountsSection() {
  const [list, setList] = useState<ag.LoginAccount[] | null>(null)
  const [err, setErr] = useState('')
  const [adding, setAdding] = useState(false)
  const [agent, setAgent] = useState('claude')
  const [id, setId] = useState('')
  const [label, setLabel] = useState('')
  const [copy, setCopy] = useState(true)
  const [busy, setBusy] = useState(false)
  const [made, setMade] = useState<{ id: string; login_command: string; copied_settings: string[] } | null>(null)
  const [copied, setCopied] = useState('')
  const load = useCallback(() => { void ag.accounts().then(x => { setList(x.accounts); setErr('') }).catch(e => setErr(msg(e))) }, [])
  useEffect(() => { load() }, [load])
  const add = async () => {
    setErr(''); setBusy(true)
    try { setMade(await ag.addAccount({ id: id.trim(), agent, label: label.trim() || undefined, copy_settings: copy })); setId(''); setLabel(''); setAdding(false); load() } catch (e) { setErr(msg(e)) } finally { setBusy(false) }
  }
  const remove = async (a: ag.LoginAccount) => {
    if (!confirm(tr('Remove account {id}? Its folder and login are kept on disk.', { id: a.id }))) return
    try { await ag.removeAccount(a.id); load() } catch (e) { setErr(msg(e)) }
  }
  const copy_ = async (cmd: string) => { try { await navigator.clipboard.writeText(cmd); setCopied(cmd); setTimeout(() => setCopied(''), 1500) } catch { /* 沒有剪貼簿權限就讓使用者自己選取 */ } }
  const state = (s: ag.LoginStatus | null) => s?.logged_in === true ? <span className="tag ok">{tr('logged in')}</span> : s?.logged_in === false ? <span className="tag warn">{tr('not logged in')}</span> : <span className="tag">{tr('unknown')}</span>
  return (
    <section aria-label="accounts">
      <div className="row page-head"><h3>{tr('Accounts')}</h3><span className="actions"><button onClick={load}>{tr('Refresh')}</button><button className="primary" onClick={() => { setAdding(a => !a); setMade(null) }}>{tr('＋ Add account')}</button></span></div>
      <p className="muted small">{tr('Each Claude or Codex account keeps its own login in its own folder. Pick one for a role or a room; the default login is what the agent uses on its own. aaf never reads or copies credentials: log in with the command shown.')}</p>
      {err && <p role="alert" className="err small">{err}</p>}
      {adding && (
        <div className="card edit">
          <label>{tr('Agent')}{' '}<select aria-label="new account agent" value={agent} onChange={e => setAgent(e.target.value)}><option value="claude">Claude</option><option value="codex">Codex</option></select></label>
          <label>{tr('Account id')}{' '}<input aria-label="new account id" value={id} placeholder="work" onChange={e => setId(e.target.value)} /></label>
          <label>{tr('Label (optional)')}{' '}<input aria-label="new account label" value={label} onChange={e => setLabel(e.target.value)} /></label>
          <label className="inline small"><input type="checkbox" checked={copy} onChange={e => setCopy(e.target.checked)} /> {tr('Copy settings (not the login) from the default account')}</label>
          <div className="row"><button className="primary" disabled={busy || !id.trim()} onClick={() => void add()}>{tr('Add')}</button></div>
        </div>
      )}
      {made && (
        <div className="card">
          <p><strong>{tr('Account {id} created. Now log in from a terminal:', { id: made.id })}</strong></p>
          <code className="block mono small" style={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>{made.login_command}</code>{' '}<button onClick={() => void copy_(made.login_command)}>{copied === made.login_command ? tr('Copied') : tr('Copy')}</button>
          {made.copied_settings.length > 0 && <p className="muted small">{tr('Copied settings: {files}', { files: made.copied_settings.join(', ') })}</p>}
        </div>
      )}
      {!list ? <p className="muted">{tr('Loading…')}</p> : (
        <div className="cards">
          {list.map(a => (
            <div key={`${a.agent}:${a.id}`} className="card" aria-label={`account ${a.id}`} style={{ minWidth: 0, overflowWrap: 'anywhere' }}>
              <div className="row"><span><strong>{a.label || (a.default ? tr('Default login') : a.id)}</strong> <span className="muted">{a.agent}{a.default ? '' : ` · ${a.id}`}</span></span>{state(a.status)}</div>
              <div className="muted small">{[a.status?.plan ?? a.quota?.plan, a.status?.email, a.status?.method && a.agent === 'codex' ? a.status.method : null].filter(Boolean).join(' · ')}</div>
              {a.agent === 'claude' && a.status?.logged_in
                ? <Usage q={{ windows: a.quota?.windows ?? [], error: a.quota?.error, observed_at: a.quota?.observed_at, from_turn: a.quota?.from_turn }} loading={false} agent="claude" account={a.id} note={a.quota?.note} />
                : a.quota && (a.quota.windows?.length || a.quota.error) ? <Usage q={{ windows: a.quota.windows ?? [], error: a.quota.error }} loading={false} /> : a.quota?.note ? <div className="muted small">{a.quota.note}</div> : null}
              <div className="muted small">{a.used_by.length ? tr('used by {roles}', { roles: a.used_by.join(', ') }) : tr('not used by any role')}</div>
              {a.status?.logged_in === false && (
                <div className="small"><div className="muted">{tr('Log in from a terminal:')}</div><code className="block mono" style={{ whiteSpace: 'pre-wrap' }}>{a.login_command}</code> <button onClick={() => void copy_(a.login_command)}>{copied === a.login_command ? tr('Copied') : tr('Copy')}</button></div>
              )}
              {!a.default && <div className="row"><button className="ghost danger" onClick={() => void remove(a)}>{tr('Remove')}</button></div>}
            </div>
          ))}
        </div>
      )}
    </section>
  )
}
