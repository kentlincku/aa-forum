import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 打包結果由 aaf 執行檔內嵌，於 /app/ 提供。`npm run dev` 把 /api 代理到執行中的 aaf。
const backend = process.env.AAF_DEV_BACKEND || 'http://127.0.0.1:8111'

export default defineConfig({
  base: '/app/',
  plugins: [react()],
  server: { proxy: { '/api': backend } },
})
