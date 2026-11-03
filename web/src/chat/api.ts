import { call, post, patch } from '../api'

export type Room = { id: number; name: string; state: string; frozen: boolean; folder: string; pinned: boolean; created_by: string; created_at: number; deleted_at: number | null; deleted_by: string | null }
export type Attachment = { id: number; room: number; message: number | null; uploader: string; kind: 'image' | 'file'; name: string; mime: string; size: number; width: number | null; height: number | null; created_at: number }
export type Message = { id: number; room: number; author: string; author_kind: 'user' | 'role' | 'system'; body: string; reply_to: number | null; thread_id: number | null; created_at: number; attachments: Attachment[] }
export type Member = { member: string; kind: 'user' | 'role' }
export type PermOption = { id: string; name: string; kind: string }
export type PermRequest = { type: 'permission_requested'; request_id: number; title: string; options: PermOption[] }
/** stuck：lane 被擋住、要 owner 解除時的原因（例如重啟前有未提交的嘗試）；正常為 null */
export type LaneInfo = { role: string; status: 'offline' | 'idle' | 'working' | 'blocked'; pending_permissions: PermRequest[]; stuck?: string | null }
export type RoleInfo = { name: string; manual: boolean; room_lanes: boolean }
export type Unread = { room: number; unread: number; mentions: number }

export type ChatSettings = { agent_chain: number; agent_chain_source: 'page' | 'config'; agent_chain_config: number; agent_chain_max: number }
export const settings = () => call<ChatSettings>('/chat/settings')
/** null＝清掉頁面值、回到設定檔（.aaf.env）的值 */
export const setSettings = (b: { agent_chain: number | null }) =>
  call<ChatSettings>('/chat/settings', { method: 'PUT', body: JSON.stringify(b), headers: { 'Content-Type': 'application/json' } })

const R = '/chat/rooms'
export const rooms = (state = 'active') => call<Room[]>(`${R}?state=${state}`)
export const roles = () => call<RoleInfo[]>('/chat/roles')
export const unread = () => call<Unread[]>('/chat/unread')
export const room = (id: number) => call<{ room: Room; members: Member[]; lead?: string | null }>(`${R}/${id}`)
export const setLead = (id: number, role: string | null) => call<{ lead: string | null }>(`${R}/${id}/lead`, { method: 'PUT', body: JSON.stringify({ role }) })
export const messages = (id: number, after = 0) => call<Message[]>(`${R}/${id}/messages?after=${after}&limit=500`)
export const latestMessages = (id: number) => call<Message[]>(`${R}/${id}/messages?latest=true&limit=300`)
export const messagesAround = (id: number, center: number) => call<Message[]>(`${R}/${id}/messages?around=${center}`)
export const send = (id: number, body: string, reply_to: number | null, attachments: number[] = []) => post<{ message: Message; delivered_to: string[] }>(`${R}/${id}/messages`, { body, reply_to, attachments })
export const attachmentUrl = (room: number, id: number) => `/api/v1${R}/${room}/attachments/${id}`
/** 原始位元組上傳（非 multipart）；進度用 XHR 回報。 */
export function upload(room: number, file: File, onProgress?: (p: number) => void): Promise<Attachment> {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest()
    x.open('POST', `/api/v1${R}/${room}/attachments?name=${encodeURIComponent(file.name || 'pasted.png')}`)
    x.withCredentials = true
    x.setRequestHeader('Content-Type', 'application/octet-stream')
    x.upload.onprogress = e => { if (e.lengthComputable) onProgress?.(e.loaded / e.total) }
    x.onload = () => {
      let j: any = null
      try { j = JSON.parse(x.responseText) } catch { /* ignore */ }
      if (x.status >= 200 && x.status < 300) resolve(j)
      else reject(new Error(j?.message ?? `upload failed (${x.status})`))
    }
    x.onerror = () => reject(new Error('upload failed (network)'))
    x.send(file)
  })
}
export const humanSize = (n: number) => n < 1024 ? `${n} B` : n < 1048576 ? `${Math.round(n / 1024)} KB` : `${(n / 1048576).toFixed(1)} MB`
export const createRoom = (name: string, members: string[]) => post<Room>(R, { name, members })
export const invite = (id: number, member: string) => post(`${R}/${id}/members`, { member })
/** 移除成員：歷史訊息保留，這人在本群還沒投遞的信取消；agent 工作中會被拒絕（先按停止） */
export const removeMember = (id: number, member: string) => call(`${R}/${id}/members/${encodeURIComponent(member)}`, { method: 'DELETE' })
export const markRead = (id: number, through: number) => post(`${R}/${id}/read`, { through })
export const manage = (id: number, body: Partial<{ name: string; folder: string; pinned: boolean; frozen: boolean; reason: string }>) => patch<Room>(`${R}/${id}`, body)
export const setState = (id: number, action: 'archive' | 'delete' | 'restore') => post<Room>(`${R}/${id}/state`, { action })
export const purge = (id: number, confirm_name: string, delete_transcripts = false) => post<{ purged: number; transcripts_deleted: boolean }>(`${R}/${id}/purge`, { confirm_name, delete_transcripts })
export const exportTranscripts = () => post<{ rooms: number; written: string[] }>('/chat/transcripts/export', {})
export const lanes = (id: number) => call<{ lanes: LaneInfo[] }>(`${R}/${id}/lanes`)
export const cancel = (id: number, role: string) => post(`${R}/${id}/lanes/${role}/cancel`)
/** owner 解除卡住的 lane：保留 session（forget=false），排隊中的訊息會重新派工 */
export const recover = (id: number, role: string) => post(`${R}/${id}/lanes/${role}/close?forget=false`)
export const answer = (id: number, role: string, request_id: number, option_id: string | null) => post(`${R}/${id}/lanes/${role}/permission`, { request_id, option_id })

export type TaskState = 'open' | 'claimed' | 'done' | 'blocked' | 'cancelled'
export type Task = { id: number; room: number; title: string; spec: string; owner: string; assignee: string | null; state: TaskState; lease_until: number | null; result: string; message: number | null; created_at: number; updated_at: number }
export const roomTasks = (id: number) => call<Task[]>(`${R}/${id}/tasks`)
export const allTasks = (mine: boolean, state?: string) => call<Task[]>(`/chat/tasks?mine=${mine}${state ? `&state=${state}` : ''}`)
export const createTask = (id: number, title: string, spec: string, assignee: string | null) => post<Task>(`${R}/${id}/tasks`, { title, spec, assignee })
export type Stalled = Task & { idle_secs: number; kind: 'stalled' | 'waiting_external'; wake_target: string | null; wake_note: string | null; last_woken_at: number | null }
export const stalledTasks = (id: number) => call<Stalled[]>(`${R}/${id}/stalled`)
export const wakeTask = (id: number, tid: number) => post<Stalled>(`${R}/${id}/tasks/${tid}/wake`)
export const claimTask = (id: number) => post<Task>(`/chat/tasks/${id}/claim`)
export const reportTask = (id: number, state: 'done' | 'blocked' | 'cancelled', result: string) => post<Task>(`/chat/tasks/${id}/report`, { state, result })

export type Approval = { room: number; on: boolean; until: string | null; by: string | null; at: number | null; expired: boolean }
export const approval = (id: number) => call<Approval>(`${R}/${id}/approval`)
export const setApproval = (id: number, on: boolean, until: string | null, reason: string) => post<{ approval: Approval; changed: boolean }>(`${R}/${id}/approval`, { on, until, reason })
export const folders = () => call<{ name: string; rooms: number }[]>('/chat/folders')

export type Hit = { id: number; room: number; room_name: string; author: string; author_kind: string; created_at: number; snippet: string }
export type SearchOpts = { q: string; room?: number; from?: string; mentions?: boolean; has_file?: boolean; since?: number; until?: number; before?: number }
export const search = (o: SearchOpts) => {
  const p = new URLSearchParams()
  for (const [k, v] of Object.entries(o)) if (v !== undefined && v !== '' && v !== false) p.set(k, String(v))
  return call<{ hits: Hit[]; more: boolean; terms: string[] }>(`/chat/search?${p}`)
}
