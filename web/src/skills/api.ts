import { call, post } from '../api'

export type SkillInfo = { id: string; description: string; packs: string[]; used_by: string[]; bytes: number; modified: number }
export type Assign = { skills: string[]; packs: string[] }
export type Listing = { skills: SkillInfo[]; packs: Record<string, string[]>; roles: Record<string, Assign>; rooms: Record<string, Assign>; dir: string }
export type Version = { version: string; bytes: number }
export type LogEntry = { at: number; actor: string; action: string; skill: string; note: string }

const put = <T>(p: string, b: unknown) => call<T>(p, { method: 'PUT', body: JSON.stringify(b) })
const del = <T>(p: string) => call<T>(p, { method: 'DELETE' })
const enc = encodeURIComponent

export const list = () => call<Listing>('/skills/items')
export const read = (id: string) => call<{ id: string; content: string }>(`/skills/items/${enc(id)}`)
export const create = (id: string, content: string) => post('/skills/items', { id, content })
export const save = (id: string, content: string, note: string) => put<{ changed: boolean }>(`/skills/items/${enc(id)}`, { content, note })
export const remove = (id: string) => del(`/skills/items/${enc(id)}`)
export const history = (id: string) => call<{ versions: Version[]; log: LogEntry[] }>(`/skills/items/${enc(id)}/history`)
export const version = (id: string, v: string) => call<{ content: string }>(`/skills/items/${enc(id)}/history/${enc(v)}`)
export const restore = (id: string, v: string) => post(`/skills/items/${enc(id)}/restore`, { version: v })
export const setPack = (name: string, skills: string[]) => put(`/skills/packs/${enc(name)}`, { skills })
export const deletePack = (name: string) => del(`/skills/packs/${enc(name)}`)
export const assign = (kind: 'role' | 'room', key: string, a: Assign) => put<Assign>(`/skills/assign/${kind}/${enc(key)}`, a)
export const preview = (role: string, room: number | null) => call<{ skills: string[]; context: string }>(`/skills/preview?role=${enc(role)}${room ? `&room=${room}` : ''}`)
