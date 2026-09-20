import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'
import { engineControlPlugin } from './vite/engineControlPlugin.ts'

export default defineConfig({
  plugins: [react(), engineControlPlugin()],
  test: {
    environment: 'jsdom',
    restoreMocks: true,
  },
})
