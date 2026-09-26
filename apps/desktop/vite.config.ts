import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    target: 'es2023',
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
  },
})
