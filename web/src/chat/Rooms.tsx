import { t as tr } from '../i18n'
import { useCallback, useEffect, useMemo, useRef, useState, type ClipboardEvent, type FormEvent, type KeyboardEvent, type UIEvent } from 'react'
import { subscribe, type Envelope, type User } from '../api'
import * as chat from './api'
import { TeamPanel } from './TeamPanel'
import { RoomTasks } from './Tasks'
import { Markdown } from '../ui/Markdown'
import { Avatar, MoreMenu } from '../ui/bits'
import * as ag from '../agents/api'

export function Rooms({ user, roomId, focus, go }: { user: User; roomId: number | null; focus?: number | null; go: (path: string) => void }) {
  const [list, setList] = useState<chat.Room[]>([])
  const [unread, setUnread] = useState<Record<number, chat.Unread>>({})
  const [creating, setCreating] = useState(false)
  const [err, setErr] = useState('')
  // 窄畫面：群組列表收起，由標題列「☰ 群組」叫出左側抽屜；選了群組就收起
  const [listOpen, setListOpen] = useState(false)
  const pick = (id: number) => { setListOpen(false); go(`/rooms/${id}`) }

  const refresh = useCallback(async () => {
    try {
      const [rs, us] = await Promise.all([chat.rooms(), chat.unread()])
      setList(rs)
      setUnread(Object.fromEntries(us.map(u => [u.room, u])))
    } catch (e) { setErr(String(e)) }
  }, [])
  useEffect(() => { void refresh() }, [refresh])
  useEffect(() => subscribe(e => { if (e.kind.startsWith('chat.room.') || e.kind === 'chat.message.created' || e.kind === 'chat.member.added' || e.kind === 'chat.member.removed') void refresh() }), [refresh])

  const groups = useMemo(() => {
    const g = new Map<string, chat.Room[]>()
    for (const r of list) { const k = r.folder || ''; g.set(k, [...(g.get(k) ?? []), r]) }
    return [...g.entries()].sort(([a], [b]) => a.localeCompare(b))
  }, [list])

  return (
    <div className={`rooms ${roomId ? '' : 'no-room'}`}>
      {listOpen && <div className="side-backdrop" onClick={() => setListOpen(false)} />}
      <aside className={`room-list ${listOpen ? 'open' : ''}`}>
        <div className="row"><strong>{tr("Rooms")}</strong><button onClick={() => setCreating(true)} aria-label="new room">＋</button></div>
        <form onSubmit={e => { e.preventDefault(); const q = (e.currentTarget.elements.namedItem('q') as HTMLInputElement).value.trim(); if (q) go(`/search?q=${encodeURIComponent(q)}`) }}>
          <input name="q" aria-label="search messages" placeholder={tr("Search messages…")} />
        </form>
        {err && <p role="alert" className="err">{err}</p>}
        {groups.map(([folder, rs]) => (
          <div key={folder}>
            {folder && <div className="folder">{folder}</div>}
            {rs.map(r => {
              const u = unread[r.id]
              return (
                <button key={r.id} className={`room-item ${r.id === roomId ? 'active' : ''}`} onClick={() => pick(r.id)}>
                  <span>{r.pinned ? '📌 ' : ''}{r.frozen ? '🧊 ' : ''}{r.name}</span>
                  {u && u.unread > 0 && r.id !== roomId && <span className={`badge ${u.mentions ? 'mention' : ''}`}>{u.mentions ? '@' : ''}{u.unread}</span>}
                </button>
              )
            })}
          </div>
        ))}
        {list.length === 0 && <p className="muted">{tr("No rooms yet.")}</p>}
      </aside>
      {creating && <CreateRoom onClose={() => setCreating(false)} onCreated={r => { setCreating(false); void refresh(); go(`/rooms/${r.id}`) }} />}
      {roomId ? <Conversation key={roomId} user={user} roomId={roomId} focus={focus ?? null} onChanged={refresh} go={go} openList={() => setListOpen(true)} /> : <main className="empty muted">{tr("Select a room")}</main>}
    </div>
  )
}

function CreateRoom({ onClose, onCreated }: { onClose: () => void; onCreated: (r: chat.Room) => void }) {
  const [name, setName] = useState('')
  const [roles, setRoles] = useState<chat.RoleInfo[]>([])
  const [picked, setPicked] = useState<string[]>([])
  const [err, setErr] = useState('')
  useEffect(() => { void chat.roles().then(setRoles).catch(e => setErr(String(e))) }, [])
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    try { onCreated(await chat.createRoom(name, picked)) } catch (x) { setErr(x instanceof Error ? x.message : String(x)) }
  }
  return (
    <div className="modal" role="dialog" aria-label="Create room">
      <form onSubmit={submit} className="card">
        <h3>{tr("New room")}</h3>
        <input aria-label="room name" placeholder={tr("name")} value={name} onChange={e => setName(e.target.value)} autoFocus />
        <fieldset>
          <legend>{tr("Agents")}</legend>
          {roles.filter(r => !r.manual).map(r => (
            <label key={r.name}><input type="checkbox" checked={picked.includes(r.name)} onChange={e => setPicked(p => e.target.checked ? [...p, r.name] : p.filter(x => x !== r.name))} /> {r.name}</label>
          ))}
          {roles.length === 0 && <span className="muted">{tr("No roles in deploy/roles.json")}</span>}
        </fieldset>
        {err && <p role="alert" className="err">{err}</p>}
        <div className="row"><button type="button" onClick={onClose}>{tr("Cancel")}</button><button type="submit" disabled={!name.trim()}>{tr("Create")}</button></div>
      </form>
    </div>
  )
}

function Conversation({ user, roomId, focus, onChanged, go, openList }: { user: User; roomId: number; focus: number | null; onChanged: () => void; go: (p: string) => void; openList: () => void }) {
  const [info, setInfo] = useState<{ room: chat.Room; members: chat.Member[] } | null>(null)
  const [msgs, setMsgs] = useState<chat.Message[]>([])
  const [replyTo, setReplyTo] = useState<chat.Message | null>(null)
  const [err, setErr] = useState('')
  // 窄畫面：右欄（成員／任務）收起，由標題列按鈕叫出抽屜
  const [sideOpen, setSideOpen] = useState(false)
  const bottom = useRef<HTMLDivElement>(null)
  const last = useRef(0)
  // 使用者往上捲時不自動拉到底；新訊息改顯示「↓ N 則新訊息」
  // 跳到搜尋結果時一開始不算「在底部」；定位後依實際位置重算（首則定位不觸發 scroll 事件）
  const pinned = useRef(!focus)
  const scroller = useRef<HTMLDivElement>(null)
  const msgsRef = useRef<chat.Message[]>([])
  msgsRef.current = msgs
  const [unseen, setUnseen] = useState(0)
  // 已讀游標只在看到底時推進：捲在上面翻舊訊息時，新訊息仍算未讀
  const readTo = useRef(0)
  const markSeen = () => {
    const id = msgsRef.current.at(-1)?.id
    if (!id || id <= readTo.current || !info?.members.some(m => m.member === user.name)) return
    readTo.current = id
    void chat.markRead(roomId, id).then(onChanged).catch(() => undefined)
  }
  const onScroll = (e: UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget
    pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
    if (pinned.current) { setUnseen(0); markSeen() }
  }
  const toBottom = () => { pinned.current = true; setUnseen(0); bottom.current?.scrollIntoView({ block: 'end' }) }

  const load = useCallback(async () => {
    try {
      const [i, m0] = await Promise.all([chat.room(roomId), chat.latestMessages(roomId)])
      // 要跳到的訊息不在最近 500 則：改載入它附近
      const m = focus && !m0.some(x => x.id === focus) ? await chat.messagesAround(roomId, focus) : m0
      setInfo(i); setMsgs(m); last.current = m.at(-1)?.id ?? 0
    } catch (e) { setErr(e instanceof Error ? e.message : String(e)) }
  }, [roomId, focus])
  useEffect(() => { void load() }, [load])

  useEffect(() => subscribe((e: Envelope) => {
    if (e.kind === 'chat.message.created' && e.payload.message.room === roomId) {
      const m: chat.Message = e.payload.message
      setMsgs(v => v.some(x => x.id === m.id) ? v : [...v, m])
    } else if (e.kind === 'chat.message.updated' && e.payload.message.room === roomId) {
      const m: chat.Message = e.payload.message
      setMsgs(v => v.map(x => x.id === m.id ? { ...x, body: m.body } : x))
    } else if ((e.kind.startsWith('chat.room.') && (e.payload.room?.id === roomId)) || ((e.kind === 'chat.member.added' || e.kind === 'chat.member.removed') && e.payload.room === roomId)) {
      void chat.room(roomId).then(setInfo).catch(() => undefined)
    }
  }), [roomId])

  const jumped = useRef(false)
  useEffect(() => {
    if (focus && !jumped.current && msgs.some(x => x.id === focus)) {
      jumped.current = true
      const el = document.getElementById(`m${focus}`)
      el?.scrollIntoView({ block: 'center' }); el?.classList.add('flash')
      const sc = scroller.current
      pinned.current = !!sc && sc.scrollHeight - sc.scrollTop - sc.clientHeight < 80
      return
    }
    // 跳到搜尋結果後停在原位；之後有新訊息進來才捲到底
    const newest = msgs.at(-1)?.id ?? 0
    const fresh = msgs.filter(x => x.id > last.current)
    const mine = fresh.some(x => x.author === user.name)
    if (focus && jumped.current && newest === last.current) { /* 停在搜尋結果 */ }
    else if (pinned.current || mine || last.current === 0) toBottom()
    else if (fresh.length > 0) setUnseen(n => n + fresh.length)
    last.current = Math.max(last.current, newest)
    if (pinned.current) markSeen()
  }, [msgs, info, roomId, user.name, onChanged])

  const byId = useMemo(() => new Map(msgs.map(m => [m.id, m])), [msgs])
  if (err) return <main className="conv"><p role="alert" className="err">{err}</p></main>
  if (!info) return <main className="conv muted">{tr("Loading…")}</main>
  const r = info.room
  const isMember = info.members.some(m => m.member === user.name)
  const readOnly = r.state !== 'active' || !isMember

  return (
    <>
      <main className="conv">
        <header className="conv-head">
          <div className="title">
            <button className="ghost small list-toggle" aria-label="rooms" onClick={openList}>☰ {tr("Rooms")}</button>
            <h2>{r.name}</h2>
            <span className="muted small">{tr('{n} members', { n: info.members.length })}</span>
            {r.frozen && <span className="tag warn">{tr('Frozen')}</span>}
            <ApprovalTag roomId={roomId} />
            <YoloTag roomId={roomId} roles={info.members.filter(m => m.kind === 'role').map(m => m.member)} />
            {r.state !== 'active' && <span className="tag">{tr(r.state)}</span>}
          </div>
          <div className="row head-actions">
            <button className="ghost small side-toggle" aria-expanded={sideOpen} onClick={() => setSideOpen(o => !o)}>👥 {tr("Team")}</button>
            <RoomMenu user={user} room={r} onChanged={() => { void load(); onChanged() }} go={go} />
          </div>
        </header>
        {r.frozen && <div className="banner">{tr("🧊 Frozen — agents are not woken in this room.")}</div>}
        <ApprovalBanner roomId={roomId} />
        <div className="messages" role="log" aria-label="messages" onScroll={onScroll} ref={scroller}>
          {msgs.map((m, i) => {
            const parent = m.reply_to ? byId.get(m.reply_to) : undefined
            if (m.author_kind === 'system') return <div key={m.id} className="msg system" id={`m${m.id}`} role="note">{sysText(m.body)}<span className="when">{time(m.created_at)}</span></div>
            const prev = msgs[i - 1]
            const cont = !!prev && prev.author === m.author && prev.author_kind === m.author_kind && m.created_at - prev.created_at < 300 && !parent
            return (
              <article key={m.id} className={`msg ${m.author_kind} ${cont ? 'cont' : ''}`} id={`m${m.id}`}>
                <div className="gutter">{cont ? <span className="when">{time(m.created_at)}</span> : <Avatar name={m.author} kind={m.author_kind} />}</div>
                <div className="main">
                  {!cont && <div className="meta"><strong>{m.author}</strong><span className="when">{time(m.created_at)}</span></div>}
                  {parent && <a className="quote" href={`#m${parent.id}`}>↳ {parent.author}：{parent.body.replace(/[*_`#>|]+/g, '').replace(/\s+/g, ' ').slice(0, 60)}</a>}
                  {m.body && <Markdown text={m.body} />}
                  {m.attachments?.length > 0 && <Attachments room={roomId} list={m.attachments} />}
                </div>
                <div className="actions">
                  <span className="muted small">#{m.id}</span>
                  {!readOnly && <button className="ghost icon" title={tr("reply")} aria-label="reply" onClick={() => setReplyTo(m)}>↩</button>}
                </div>
              </article>
            )
          })}
          <div ref={bottom} />
        </div>
        {unseen > 0 && <button className="small new-msgs" onClick={toBottom}>{tr("↓ {n} new messages", { n: unseen })}</button>}
        {!readOnly && <Composer roomId={roomId} members={info.members} replyTo={replyTo} clearReply={() => setReplyTo(null)} />}
      </main>
      {sideOpen && <div className="side-backdrop" onClick={() => setSideOpen(false)} />}
      <SidePanel user={user} roomId={roomId} members={info.members} readOnly={readOnly} go={go} open={sideOpen} onClose={() => setSideOpen(false)} />
    </>
  )
}

type Pending = { key: string; file: File; progress: number; att?: chat.Attachment; err?: string }

function Composer({ roomId, members, replyTo, clearReply }: { roomId: number; members: chat.Member[]; replyTo: chat.Message | null; clearReply: () => void }) {
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState('')
  const [pending, setPending] = useState<Pending[]>([])
  const [drag, setDrag] = useState(false)
  const box = useRef<HTMLTextAreaElement>(null)
  const picker = useRef<HTMLInputElement>(null)
  useEffect(() => { if (replyTo) box.current?.focus() }, [replyTo])
  const roles = members.filter(m => m.kind === 'role').map(m => m.member)
  // @ 成員選單：游標前是「@前綴」且 @ 前面不是英數字（email 不觸發）
  const [mention, setMention] = useState<{ start: number; q: string } | null>(null)
  const [sel, setSel] = useState(0)
  const options = mention ? members.filter(m => m.member.toLowerCase().startsWith(mention.q.toLowerCase())) : []
  const track = (value: string, caret: number) => {
    const m = /(^|[^A-Za-z0-9._%+-])@([A-Za-z0-9_-]*)$/.exec(value.slice(0, caret))
    setMention(m ? { start: caret - m[2].length - 1, q: m[2] } : null)
    setSel(0)
  }
  const pick = (name: string) => {
    if (!mention) return
    const end = mention.start + 1 + mention.q.length
    const next = `${text.slice(0, mention.start)}@${name} ${text.slice(end)}`
    const caret = mention.start + name.length + 2
    setText(next); setMention(null)
    requestAnimationFrame(() => { box.current?.focus(); box.current?.setSelectionRange(caret, caret) })
  }
  const uploading = pending.some(p => !p.att && !p.err)
  const ready = pending.filter(p => p.att)

  const add = (files: FileList | File[]) => {
    for (const file of Array.from(files)) {
      const key = `${Date.now()}-${Math.random()}`
      setPending(v => [...v, { key, file, progress: 0 }])
      void chat.upload(roomId, file, p => setPending(v => v.map(x => x.key === key ? { ...x, progress: p } : x)))
        .then(att => setPending(v => v.map(x => x.key === key ? { ...x, att, progress: 1 } : x)))
        .catch(e => setPending(v => v.map(x => x.key === key ? { ...x, err: e instanceof Error ? e.message : String(e) } : x)))
    }
  }
  const send = async () => {
    if ((!text.trim() && ready.length === 0) || busy || uploading) return
    setBusy(true); setNote('')
    try {
      const r = await chat.send(roomId, text, replyTo?.id ?? null, ready.map(p => p.att!.id))
      setText(''); setPending([]); clearReply()
      setNote(r.delivered_to.length ? `→ ${r.delivered_to.join(', ')}` : 'no agent woken')
    } catch (e) { setNote(e instanceof Error ? e.message : String(e)) } finally { setBusy(false) }
  }
  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (mention && options.length > 0 && !e.nativeEvent.isComposing) {
      if (e.key === 'ArrowDown') { e.preventDefault(); setSel(i => (i + 1) % options.length); return }
      if (e.key === 'ArrowUp') { e.preventDefault(); setSel(i => (i - 1 + options.length) % options.length); return }
      if (e.key === 'Enter' || e.key === 'Tab') { e.preventDefault(); pick(options[Math.min(sel, options.length - 1)].member); return }
      if (e.key === 'Escape') { e.preventDefault(); setMention(null); return }
    }
    // IME 組字中的 Enter 不送出
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send() }
    if (e.key === 'Escape' && replyTo) clearReply()
  }
  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = Array.from(e.clipboardData.files)
    if (files.length) { e.preventDefault(); add(files) }
  }
  return (
    <div className={`composer ${drag ? 'drag' : ''}`}
      onDragOver={e => { if (e.dataTransfer.types.includes('Files')) { e.preventDefault(); setDrag(true) } }}
      onDragLeave={() => setDrag(false)}
      onDrop={e => { if (e.dataTransfer.files.length) { e.preventDefault(); setDrag(false); add(e.dataTransfer.files) } }}>
      {replyTo && <div className="replying">{tr("↳ replying to #")}{replyTo.id} {replyTo.author} <button className="link" onClick={clearReply}>×</button></div>}
      {pending.length > 0 && (
        <div className="pending" aria-label="attachments to send">
          {pending.map(p => (
            <span key={p.key} className={`chip ${p.err ? 'bad' : ''}`} title={p.err ?? ''}>
              {p.file.type.startsWith('image/') ? '🖼' : '📎'} {p.file.name || 'pasted image'} · {chat.humanSize(p.file.size)}
              {!p.att && !p.err && <> · {Math.round(p.progress * 100)}%</>}
              {p.err && <> · {p.err}</>}
              <button className="link" aria-label={`remove ${p.file.name}`} onClick={() => setPending(v => v.filter(x => x.key !== p.key))}>×</button>
            </span>
          ))}
        </div>
      )}
      {mention && options.length > 0 && (
        <ul className="mention-menu" role="listbox" aria-label="mention members">
          {options.map((m, i) => (
            <li key={m.member} role="option" aria-selected={i === sel} className={i === sel ? 'active' : ''}
              onMouseDown={e => { e.preventDefault(); pick(m.member) }}>@{m.member} <span className="muted small">{m.kind === 'role' ? tr(' (agent)') : ''}</span></li>
          ))}
        </ul>
      )}
      <textarea ref={box} aria-label="message" rows={3} value={text} onChange={e => { setText(e.target.value); track(e.target.value, e.target.selectionStart) }}
        onClick={e => track(e.currentTarget.value, e.currentTarget.selectionStart)} onBlur={() => setTimeout(() => setMention(null), 150)} onKeyDown={onKey} onPaste={onPaste}
        placeholder={roles.length ? tr('Message… (@{roles}) — paste or drop files', { roles: roles.join(' @') }) : tr('Message… — paste or drop files')} />
      <div className="row">
        <span className="row"><button aria-label="attach files" title={tr("Attach files")} onClick={() => picker.current?.click()}>📎</button>
          <input ref={picker} type="file" multiple hidden aria-label="file picker" onChange={e => { if (e.target.files) add(e.target.files); e.target.value = '' }} />
          <span className="muted">{note}</span></span>
        <button onClick={() => void send()} disabled={busy || uploading || (!text.trim() && ready.length === 0)}>{uploading ? tr('Uploading…') : tr('Send')}</button>
      </div>
    </div>
  )
}

/** 訊息附件：圖片顯示縮圖（點開原圖），檔案顯示名稱與大小（下載）。 */
function Attachments({ room, list }: { room: number; list: chat.Attachment[] }) {
  return (
    <div className="attachments">
      {list.map(a => a.kind === 'image'
        ? <a key={a.id} href={chat.attachmentUrl(room, a.id)} target="_blank" rel="noopener noreferrer" className="thumb" title={a.name}>
            <img src={chat.attachmentUrl(room, a.id)} alt={a.name} loading="lazy" width={a.width ?? undefined} height={a.height ?? undefined} />
          </a>
        : <a key={a.id} href={chat.attachmentUrl(room, a.id)} download={a.name} className="file-chip">📎 {a.name} <span className="muted">{chat.humanSize(a.size)}</span></a>)}
    </div>
  )
}

function RoomMenu({ user, room, onChanged, go }: { user: User; room: chat.Room; onChanged: () => void; go: (p: string) => void }) {
  const [err, setErr] = useState('')
  const owner = user.rank === 'owner'
  const run = async (f: () => Promise<unknown>) => { setErr(''); try { await f(); onChanged() } catch (e) { setErr(e instanceof Error ? e.message : String(e)) } }
  const freeze = () => {
    const reason = prompt(room.frozen ? tr('Reason to unfreeze') : tr('Reason to freeze (stops waking agents)'))
    if (reason?.trim()) void run(() => chat.manage(room.id, { frozen: !room.frozen, reason }))
  }
  const del = () => { if (confirm(tr('Move "{name}" to the trash?', { name: room.name }))) void run(async () => { await chat.setState(room.id, 'delete'); go('/rooms') }) }
  return (
    <div className="row menu">
      {err && <span role="alert" className="err small">{err}</span>}
      <MoreMenu label="room actions">
        {room.state === 'active' && <button role="menuitem" onClick={() => void run(() => chat.manage(room.id, { pinned: !room.pinned }))}>{room.pinned ? tr('Unpin') : tr('Pin')}</button>}
        {room.state === 'active' && <button role="menuitem" onClick={() => void run(async () => {
          const f = prompt(tr('Folder (empty to remove)'), room.folder ?? '')
          if (f !== null) await chat.manage(room.id, { folder: f })
        })}>{tr("Folder")}</button>}
        {room.state === 'active' && <ApprovalButton roomId={room.id} onError={setErr} />}
        {room.state === 'active' && <button role="menuitem" onClick={freeze}>{room.frozen ? tr('Unfreeze') : tr('Freeze')}</button>}
        {owner && <button role="menuitem" title={tr("Write Markdown transcripts for all rooms now")} onClick={() => void run(async () => { const x = await chat.exportTranscripts(); alert(tr('Transcripts: {rooms} rooms, {files} files updated', { rooms: x.rooms, files: x.written.length })) })}>{tr("Export transcripts")}</button>}
        {owner && room.state === 'active' && <button role="menuitem" onClick={() => void run(() => chat.setState(room.id, 'archive'))}>{tr("Archive")}</button>}
        {owner && room.state === 'archived' && <button role="menuitem" onClick={() => void run(() => chat.setState(room.id, 'restore'))}>{tr("Restore")}</button>}
        {owner && room.state !== 'deleted' && <><hr /><button role="menuitem" className="danger" onClick={del}>{tr("Delete")}</button></>}
      </MoreMenu>
    </div>
  )
}

/** 右欄分頁：Team／Tasks；任務數量即時顯示在分頁上。 */
function SidePanel({ user, roomId, members, readOnly, go, open, onClose }: { user: User; roomId: number; members: chat.Member[]; readOnly: boolean; go: (p: string) => void; open: boolean; onClose: () => void }) {
  const [tab, setTab] = useState<'team' | 'tasks'>(() => (sessionStorage.getItem('aaf.sidetab') as 'team' | 'tasks') || 'team')
  const [active, setActive] = useState(0)
  const pick = (t: 'team' | 'tasks') => { setTab(t); sessionStorage.setItem('aaf.sidetab', t) }
  const count = useCallback(() => { void chat.roomTasks(roomId).then(ts => setActive(ts.filter(t => ['open', 'claimed', 'blocked'].includes(t.state)).length)).catch(() => undefined) }, [roomId])
  useEffect(count, [count])
  useEffect(() => subscribe(e => { if (e.kind.startsWith('chat.task.')) count() }), [count])
  return (
    <div className={`side ${open ? 'open' : ''}`}>
      <div className="tabs" role="tablist">
        <button role="tab" aria-selected={tab === 'team'} className={tab === 'team' ? 'active' : ''} onClick={() => pick('team')}>{tr("Team")}</button>
        <button role="tab" aria-selected={tab === 'tasks'} className={tab === 'tasks' ? 'active' : ''} onClick={() => pick('tasks')}>{tr("Tasks")}{active > 0 && <span className="badge">{active}</span>}</button>
        <button className="ghost icon side-close" aria-label="close panel" onClick={onClose}>✕</button>
      </div>
      {tab === 'team' ? <TeamPanel user={user} roomId={roomId} members={members} /> : <aside className="team"><RoomTasks user={user} roomId={roomId} members={members} readOnly={readOnly} go={go} /></aside>}
    </div>
  )
}

const time = (s: number) => new Date(s * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })

/** 系統公告在地化：伺服器存英文原句，前端依句型翻成目前語言；認不得的原樣顯示。 */
function sysText(body: string): string {
  let m = body.match(/^(\S+) added (.+) to the room\.(?: New members can read the full history\.)?$/)
  if (m) return tr('{by} added {who}', { by: m[1], who: m[2].split(', ').join(tr(', ')) })
  m = body.match(/^(\S+) removed (\S+) from the room\.$/)
  if (m) return tr('{by} removed {who}', { by: m[1], who: m[2] })
  m = body.match(/^Room (frozen|unfrozen) by (\S+)\.[^\n]*(?:\nReason: ([\s\S]*))?$/)
  if (m) return tr(m[1] === 'frozen' ? '{by} froze the room' : '{by} unfroze the room', { by: m[2] }) + (m[3]?.trim() ? tr(' — reason: {r}', { r: m[3].trim() }) : '')
  m = body.match(/^Auto-approve (ON|OFF) by (\S+?)(?: \((.*?)\))?\.[^\n]*(?:\nReason: ([\s\S]*))?$/)
  if (m) {
    const span = m[3] ? m[3].replace(/^until (\S+) \(inclusive\)$/, (_, d) => tr('until {d}', { d })).replace(/^with no end date.*$/, tr('no end date')) : ''
    return tr(m[1] === 'ON' ? '{by} turned auto-approve on' : '{by} turned auto-approve off', { by: m[2] }) + (span ? ` (${span})` : '') + (m[4]?.trim() ? tr(' — reason: {r}', { r: m[4].trim() }) : '')
  }
  m = body.match(/^(\S+) set (\S+) in this room: (.+), permissions: (.+)\.$/)
  if (m) return tr('{by} set {role} in this room: {what} · permissions: {perm}', { by: m[1], role: m[2], what: m[3].replace('default model', tr('default model')), perm: m[4].replace(/^YOLO until (\S+)$/, (_, d) => tr('YOLO until {d}', { d })) })
  m = body.match(/^(\S+) reset (\S+) in this room to its global settings\.$/)
  if (m) return tr('{by} reset {role} in this room to its global settings', { by: m[1], role: m[2] })
  return body
}

function YoloTag({ roomId, roles }: { roomId: number; roles: string[] }) {
  const [on, setOn] = useState<string[]>([])
  const key = roles.join(',')
  const load = useCallback(() => {
    void Promise.all(key.split(',').filter(Boolean).map(r => ag.roomOverride(roomId, r).then(v => v.yolo ? r : '').catch(() => '')))
      .then(v => setOn(v.filter(Boolean)))
  }, [roomId, key])
  useEffect(load, [load])
  useEffect(() => subscribe(e => { if (e.kind.startsWith('runtime.roster.')) load() }), [load])
  return on.length ? <span className="tag warn" title={on.join(', ')}>YOLO · {on.join(', ')}</span> : null
}

function ApprovalTag({ roomId }: { roomId: number }) {
  const a = useApproval(roomId)
  return a?.on ? <span className="tag ok">{tr('Auto-approve on')}</span> : null
}

function useApproval(roomId: number) {
  const [a, setA] = useState<chat.Approval | null>(null)
  const load = useCallback(() => { void chat.approval(roomId).then(setA).catch(() => undefined) }, [roomId])
  useEffect(load, [load])
  useEffect(() => subscribe(e => { if (e.kind === 'chat.approval.changed' && e.payload.room === roomId) load() }), [roomId, load])
  return a
}

function ApprovalBanner({ roomId }: { roomId: number }) {
  const a = useApproval(roomId)
  if (!a?.on) return null
  return <div className="banner approve">{tr("✅ Auto-approve is on")}{a.until ? ` until ${a.until}` : ''} {tr("— agent permission requests here are allowed once automatically (by")}{' '}{a.by}).</div>
}

function ApprovalButton({ roomId, onError }: { roomId: number; onError: (m: string) => void }) {
  const a = useApproval(roomId)
  const toggle = async () => {
    try {
      if (a?.on) {
        const reason = prompt('Turn off auto-approve. Reason (optional)')
        if (reason === null) return
        await chat.setApproval(roomId, false, null, reason)
      } else {
        const until = prompt('Auto-approve agent permission requests in this room.\nEnd date (YYYY-MM-DD, inclusive) or leave empty for no end date:', '')
        if (until === null) return
        const reason = prompt('Reason (optional)') ?? ''
        await chat.setApproval(roomId, true, until.trim() || null, reason)
      }
    } catch (e) { onError(e instanceof Error ? e.message : String(e)) }
  }
  return <button role="menuitem" onClick={() => void toggle()} title={tr("Auto-approve agent permission requests")}>{a?.on ? tr('Auto-approve: on') : tr('Auto-approve: off')}</button>
}
