import { call, post } from '../api'

export type AgentStatus = { id: string; label: string; acp: string[]; source: string; npm?: string; needs: string; custom: boolean
  installed: boolean; adapter_installed?: boolean; cli_installed?: boolean | null; cli?: string; command: string[] | null; logged_in: boolean | null; login_note: string }
export type RoleFile = { driver: string; acp_agent?: string; model?: string; rank?: string; label?: string; command?: string[] }
export type Catalog = { agents: AgentStatus[]; roles: Record<string, RoleFile>; vendor: string }
export type Job = { id: string; kind?: string; agent: string; npm: string; cmd: string[]; state: 'running' | 'done' | 'failed'; exit: number | null; output: string; offset: number }
export type Plan = { id: string; label: string; npm: string; acp: string; bins: string[]; install_cmd: string[] }
export type Probe = { ok: boolean; model?: string; models?: { id: string; name: string }[]; error?: string }
export type Mode = { id: string; name: string; description?: string | null }
export type LoginStatus = { logged_in: boolean | null; method?: string; plan?: string; email?: string | null; detail?: string }
/** 帳號：default 是各 agent 平常的登入；其他是 aaf 建的獨立資料夾。status 在 light 清單裡是 null。 */
export type LoginAccount = { id: string; agent: string; default: boolean; label?: string | null; dir?: string | null; login_command: string
  status: LoginStatus | null; quota: { windows?: Window[]; note?: string; error?: string; plan?: string; observed_at?: number; from_turn?: boolean } | null; used_by: string[] }
export type Models = { default: string | null; models: { id: string; name: string }[]; source: string; mode?: string | null; modes?: Mode[]; widest_mode?: string | null; effort?: string | null; efforts?: string[] }
export type Override = { acp_agent?: string; model?: string; mode?: string; effort?: string; account?: string; yolo?: boolean; yolo_until?: string }
export type RoomOverrideView = { role: string; room: number; global: { agent: string | null; model: string | null; mode: string | null; effort?: string | null; account?: string | null }
  override: Override; effective: { agent: string | null; model: string | null; mode: string | null; effort?: string | null; account?: string | null }; yolo: boolean; can_edit: boolean }
export type Turn = { id: number; role: string; room: number | null; started_at: number; ended_at: number | null; outcome: string; error: string | null; messages: number }
export type RoleHealth = { role: string; rank: string; label?: string; agent?: string; model?: string; driver?: string; transport?: string | null; effort?: string | null; account?: string | null; workdir: string
  lanes: { room: number | null; status: string }[]; last_turn: Turn | null; agent_error?: string | null; mode?: string; room_overrides?: { room: number; yolo: boolean; agent_error?: string | null }[] }
export type Window = { label: string; count?: string; remaining_percent: number; resets_at: number | null; unlimited?: boolean }
export type Account = { agent: string; engine: string; in_use: boolean; windows: Window[]; plan?: string; note?: string; error?: string; observed_at?: number; from_turn?: boolean }
export type Dashboard = { roles: number; lanes_open: number; working: number; rooms: number
  queued: { room: number; name: string; queued: number }[]
  turns_24h: Record<string, { count: number; avg_secs: number | null }>; recent: Turn[]; alerts: Turn[] }

const del = <T>(p: string) => call<T>(p, { method: 'DELETE' })
const enc = encodeURIComponent

export const catalog = () => call<Catalog>('/agents/catalog')
export const planCustom = (npm: string) => post<Plan>('/agents/plan-custom', { npm })
export const install = (b: { agent?: string; npm?: string }) => post<Job>('/agents/install', { ...b, confirm: true })
export const job = (id: string, offset: number) => call<Job>(`/agents/install/${enc(id)}?offset=${offset}`)
export const probe = (agent: string) => post<Probe>('/agents/probe', { agent })
export const models = (agent: string) => call<Models>(`/agents/models/${enc(agent)}`)
export const addRole = (b: { role: string; agent: string; model?: string; rank: string; label?: string; skills: string[] }) => post('/agents/roles', b)
export const setEngine = (role: string, b: { agent?: string; model?: string; rank?: string; label?: string; mode?: string; test?: boolean; transport?: 'acp' | 'headless'; effort?: string; account?: string }) =>
  post<{ check: { ok: boolean; model?: string; connect_only?: boolean } | null }>(`/agents/roles/${enc(role)}/engine`, b)
export const accounts = (light = false) => call<{ accounts: LoginAccount[]; root: string }>(`/agents/accounts${light ? '?light=true' : ''}`)
export const addAccount = (b: { id: string; agent: string; label?: string; copy_settings?: boolean }) =>
  post<{ id: string; dir: string; login_command: string; copied_settings: string[]; next: string }>('/agents/accounts', b)
export const removeAccount = (id: string) => del<{ removed: string; dir_kept: string }>(`/agents/accounts/${enc(id)}`)
export const removeRole = (role: string) => del(`/agents/roles/${enc(role)}`)
export const roomOverride = (room: number, role: string) => call<RoomOverrideView>(`/agents/rooms/${room}/roles/${enc(role)}`)
export const setRoomOverride = (room: number, role: string, b: { agent: string; model: string; mode: string; effort?: string; account?: string; yolo: boolean; yolo_until: string; test?: boolean }) =>
  call(`/agents/rooms/${room}/roles/${enc(role)}`, { method: 'PUT', body: JSON.stringify(b), headers: { 'Content-Type': 'application/json' } })
export const resetRoomOverride = (room: number, role: string) => del(`/agents/rooms/${room}/roles/${enc(role)}`)
export type Engine = { source: 'bundled' | 'cli'; package?: string | null; version: string | null; bin: string | null; managed: boolean; official?: string | null; latest?: string | null; latest_error?: string | null }
export type Compat = { state: 'ok' | 'fail' | 'unknown'; at?: number; secs?: number; error?: string | null; adapter?: string | null; engine?: string | null; how?: string; last?: Compat | null }
export type Package = { agent: string; label: string; custom: boolean; npm: string | null; pinned: string | null; installed: string | null; engine: Engine | null; compat: Compat | null
  legacy: { npm: string; version: string }[]; latest: string | null; deprecated: string | null; checked_at: number | null; check_error: string | null; used_by: string[] }
export const packages = () => call<{ packages: Package[] }>('/agents/packages')
export const checkPackages = () => post<{ packages: Package[] }>('/agents/packages/check', {})
export const engineUpgrade = (agent: string, version: string) => post<Job>('/agents/packages/engine-upgrade', { agent, version, confirm: true })
export const testCompat = (agent: string) => post<{ ok: boolean; secs?: number; error?: string; model?: string }>('/agents/packages/test', { agent })
export const upgrade = (agent: string, version?: string) => post<Job>('/agents/packages/upgrade', { agent, version, confirm: true })
/** Claude 額度：送一個一個字的回合，從回合附帶的額度資訊讀（會用掉一點點額度） */
export const refreshClaudeQuota = (account?: string) => post<{ account: string; observed_at: number; windows: Window[] }>('/agents/quota/refresh', { account })
export const quota = () => call<{ accounts: Account[] }>('/agents/quota')
export const health = () => call<{ roles: RoleHealth[] }>('/agents/health')
export const dashboard = () => call<Dashboard>('/agents/dashboard')
