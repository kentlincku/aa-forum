// API 用戶端：cookie 登入（HttpOnly），不在前端保存 token。
export type User = { name: string; rank: string }
export type Meta = { version: string; features: string[]; nav: { feature: string; label: string; path: string }[] }
export type Envelope = { seq: number; kind: string; version: number; payload: any }

export class ApiError extends Error {
  constructor(public status: number, message: string) { super(message) }
}

export async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await fetch(`/api/v1${path}`, { credentials: 'same-origin', ...init, headers: { 'Content-Type': 'application/json', ...init?.headers } })
  const body = await r.json().catch(() => ({}))
  if (!r.ok) throw new ApiError(r.status, body.message ?? r.statusText)
  return body as T
}
export const post = <T>(path: string, body?: unknown) => call<T>(path, { method: 'POST', body: JSON.stringify(body ?? {}) })
export const patch = <T>(path: string, body: unknown) => call<T>(path, { method: 'PATCH', body: JSON.stringify(body) })

export const setupState = () => call<{ needs_setup: boolean }>('/auth/setup')
export const setup = (name: string, password: string) => post('/auth/setup', { name, password })
export const login = (name: string, password: string) => post('/auth/login', { name, password })
export const logout = () => post('/auth/logout')
export const me = () => call<User>('/auth/me')
export const meta = () => call<Meta>('/meta')

type Listener = (e: Envelope) => void
const listeners = new Set<Listener>()
let source: EventSource | null = null

/** 單一 SSE 連線，多個元件共用；斷線自動重連並帶 Last-Event-ID 補送。 */
export function subscribe(fn: Listener): () => void {
  listeners.add(fn)
  if (!source) {
    source = new EventSource('/api/v1/events')
    source.onmessage = (m: MessageEvent) => {
      let e: Envelope
      try { e = JSON.parse(m.data) } catch { return }
      listeners.forEach(l => l(e))
    }
  }
  return () => {
    listeners.delete(fn)
    if (listeners.size === 0 && source) { source.close(); source = null }
  }
}
