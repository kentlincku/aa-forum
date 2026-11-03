// 外觀：主題 × 明暗 × 字級。存在本機（localStorage），只換 CSS 變數、不換版面。
// 在 render 前套用（main.tsx 先 import），避免載入時閃一下。
export type Theme = 'default' | 'paper' | 'contrast' | 'terminal'
export type Scheme = 'system' | 'light' | 'dark'
export type Size = 's' | 'm' | 'l'
export type Look = { theme: Theme; scheme: Scheme; size: Size }

const KEY = 'aaf.look'
const THEMES: Theme[] = ['default', 'paper', 'contrast', 'terminal']
const SCHEMES: Scheme[] = ['system', 'light', 'dark']
const SIZES: Size[] = ['s', 'm', 'l']

export function load(): Look {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? '{}')
    return {
      theme: THEMES.includes(v.theme) ? v.theme : 'default',
      scheme: SCHEMES.includes(v.scheme) ? v.scheme : 'system',
      size: SIZES.includes(v.size) ? v.size : 'm',
    }
  } catch { return { theme: 'default', scheme: 'system', size: 'm' } }
}

const media = window.matchMedia('(prefers-color-scheme: dark)')

export function apply(l: Look) {
  const el = document.documentElement
  el.dataset.theme = l.theme
  el.dataset.size = l.size
  el.dataset.mode = l.scheme === 'system' ? (media.matches ? 'dark' : 'light') : l.scheme
}

export function save(l: Look) {
  localStorage.setItem(KEY, JSON.stringify(l))
  apply(l)
}

apply(load())
media.addEventListener('change', () => apply(load()))
