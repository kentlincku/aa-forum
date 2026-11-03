import { call } from '../api'

export type Space = { id: string; label: string; writable: boolean; path: string; exists: boolean }
export type Entry = { rel: string; size: number; modified: number }
export type Listing = { space: string; files: Entry[]; total: number; offset: number; limit: number; partial: boolean }
export type Uploaded = { file: { original_name: string; stored_name: string; renamed: boolean; size: number; rel: string; path: string }; uploaded_at: string }

export const me = () => call<{ spaces: Space[]; upload_limit: number | null }>('/files/me')
export const list = (space: string, q: string, offset: number) =>
  call<Listing>(`/files/list?space=${encodeURIComponent(space)}&q=${encodeURIComponent(q)}&offset=${offset}`)
export const downloadUrl = (space: string, rel: string) => `/api/v1/files/download?space=${encodeURIComponent(space)}&rel=${encodeURIComponent(rel)}`

export function upload(space: string, file: File, onProgress?: (p: number) => void): Promise<Uploaded> {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest()
    x.open('POST', `/api/v1/files/upload?space=${encodeURIComponent(space)}&name=${encodeURIComponent(file.name)}`)
    x.withCredentials = true
    x.setRequestHeader('Content-Type', 'application/octet-stream')
    x.setRequestHeader('x-aaf-files', '1')
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
