import { t as tr, lang, setLang, type Lang } from './i18n'
import { useEffect, useState, type FormEvent } from 'react'
import * as api from './api'
import { Rooms } from './chat/Rooms'
import { Trash } from './chat/Trash'
import { TaskBoard } from './chat/Tasks'
import { FilesPage } from './files/Files'
import { SkillsPage } from './skills/Skills'
import { SearchPage } from './chat/Search'
import { AgentsPage, DashboardPage } from './agents/Agents'
import * as theme from './theme'

type Phase = { k: 'loading' } | { k: 'setup' } | { k: 'login' } | { k: 'in'; user: api.User; meta: api.Meta }

/** hash 路由：#/rooms、#/rooms/3、#/trash */
function useRoute(): [string, (p: string) => void] {
  const [path, setPath] = useState(() => location.hash.slice(1) || '/rooms')
  useEffect(() => {
    const f = () => setPath(location.hash.slice(1) || '/rooms')
    window.addEventListener('hashchange', f)
    return () => window.removeEventListener('hashchange', f)
  }, [])
  return [path, (p: string) => { location.hash = p }]
}

export default function App() {
  const [phase, setPhase] = useState<Phase>({ k: 'loading' })
  const refresh = async () => {
    try {
      const [user, meta] = await Promise.all([api.me(), api.meta()])
      setPhase({ k: 'in', user, meta })
    } catch {
      const s = await api.setupState().catch(() => ({ needs_setup: false }))
      setPhase({ k: s.needs_setup ? 'setup' : 'login' })
    }
  }
  useEffect(() => { void refresh() }, [])

  if (phase.k === 'loading') return <p className="page">{tr("Loading…")}</p>
  if (phase.k === 'setup' || phase.k === 'login') return <Auth mode={phase.k} onDone={refresh} />
  return <Shell user={phase.user} meta={phase.meta} onLogout={async () => { await api.logout(); await refresh() }} />
}

function Auth({ mode, onDone }: { mode: 'setup' | 'login'; onDone: () => void }) {
  const [name, setName] = useState('')
  const [password, setPassword] = useState('')
  const [err, setErr] = useState('')
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setErr('')
    try {
      await (mode === 'setup' ? api.setup(name, password) : api.login(name, password))
      onDone()
    } catch (x) { setErr(x instanceof Error ? x.message : String(x)) }
  }
  return (
    <form onSubmit={submit} className="auth card">
      <h2>{mode === 'setup' ? tr('Create owner account') : tr('Sign in')}</h2>
      <input aria-label="name" placeholder={tr("name")} value={name} onChange={e => setName(e.target.value)} autoFocus />
      <input aria-label="password" placeholder={tr("password")} type="password" value={password} onChange={e => setPassword(e.target.value)} />
      <button type="submit">{mode === 'setup' ? tr('Create') : tr('Sign in')}</button>
      {err && <p role="alert" className="err">{err}</p>}
    </form>
  )
}

/** 外殼：側欄由各 feature 註冊的 nav 產生。 */
function Shell({ user, meta, onLogout }: { user: api.User; meta: api.Meta; onLogout: () => void }) {
  const [path, go] = useRoute()
  const m = path.match(/^\/rooms(?:\/(\d+))?(?:\?m=(\d+))?/)
  // 窄畫面（≤900px）側欄收成抽屜：頂端一條列放選單鈕，換頁自動收起
  const [navOpen, setNavOpen] = useState(false)
  useEffect(() => { setNavOpen(false) }, [path])
  return (
    <div className={`shell${navOpen ? ' nav-open' : ''}`}>
      <header className="topbar">
        <button className="ghost" aria-label={tr('Menu')} aria-expanded={navOpen} onClick={() => setNavOpen(o => !o)}>☰</button>
        <strong>{tr("AA Forum")}</strong>
      </header>
      {navOpen && <div className="side-backdrop" onClick={() => setNavOpen(false)} />}
      <nav className="nav">
        <strong>{tr("AA Forum")}</strong>
        <ul>
          {meta.nav.map(n => <li key={n.path}><a href={`#${n.path}`} className={path.startsWith(n.path) ? 'active' : ''}>{tr(n.label)}</a></li>)}
        </ul>
        <div className="foot">
          <div className="muted">{user.name} ({user.rank}) · v{meta.version}</div>
          <Appearance />
          <select aria-label="language" value={lang} onChange={e => setLang(e.target.value as Lang)}>
            <option value="en">English</option>
            <option value="zh-TW">繁體中文</option>
          </select>
          <button onClick={onLogout}>{tr("Sign out")}</button>
        </div>
      </nav>
      {m ? <Rooms user={user} roomId={m[1] ? Number(m[1]) : null} focus={m[2] ? Number(m[2]) : null} go={go} />
        : path.startsWith('/trash') ? <Trash go={go} />
        : path.startsWith('/tasks') ? <TaskBoard user={user} go={go} />
        : path.startsWith('/files') ? <FilesPage user={user} />
        : path.startsWith('/skills') ? <SkillsPage user={user} />
        : path.startsWith('/agents') ? <AgentsPage user={user} />
        : path.startsWith('/dashboard') ? <DashboardPage go={go} />
        : path.startsWith('/search') ? <SearchPage go={go} initial={decodeURIComponent(path.split('?q=')[1] ?? '')} />
        : <main className="page muted">{tr("Not found")}</main>}
    </div>
  )
}

/** 外觀：主題、明暗、字級（存在本機瀏覽器）。 */
function Appearance() {
  const [look, setLook] = useState(theme.load)
  const set = (p: Partial<theme.Look>) => { const n = { ...look, ...p }; setLook(n); theme.save(n) }
  return (
    <details className="appearance">
      <summary>{tr('Appearance')}</summary>
      <label>{tr('Theme')}<select aria-label="theme" value={look.theme} onChange={e => set({ theme: e.target.value as theme.Theme })}>
        <option value="default">{tr('Default')}</option><option value="paper">{tr('Paper')}</option>
        <option value="contrast">{tr('High contrast')}</option><option value="terminal">{tr('Terminal')}</option>
      </select></label>
      <label>{tr('Light / dark')}<select aria-label="color scheme" value={look.scheme} onChange={e => set({ scheme: e.target.value as theme.Scheme })}>
        <option value="system">{tr('Follow system')}</option><option value="light">{tr('Light')}</option><option value="dark">{tr('Dark')}</option>
      </select></label>
      <label>{tr('Text size')}<select aria-label="text size" value={look.size} onChange={e => set({ size: e.target.value as theme.Size })}>
        <option value="s">{tr('Small')}</option><option value="m">{tr('Medium')}</option><option value="l">{tr('Large')}</option>
      </select></label>
    </details>
  )
}
