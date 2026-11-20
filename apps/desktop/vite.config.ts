import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'

function stripLegacyKatexFonts(): Plugin {
  return {
    name: 'strip-legacy-katex-fonts',
    generateBundle(_options, bundle) {
      for (const [fileName, output] of Object.entries(bundle)) {
        if (output.type !== 'asset') continue
        if (/\.(?:ttf|woff)$/i.test(fileName)) {
          delete bundle[fileName]
          continue
        }
        if (fileName.endsWith('.css') && typeof output.source === 'string') {
          output.source = output.source
            .replace(/,\s*url\([^)]+\.(?:woff|ttf)\)\s*format\((?:"|')?(?:woff|truetype)(?:"|')?\)/gi, '')
            .replace(/url\([^)]+\.(?:woff|ttf)\)\s*format\((?:"|')?(?:woff|truetype)(?:"|')?\)\s*,?/gi, '')
        }
      }
    },
  }
}

export default defineConfig({
  base: './',
  plugins: [react(), stripLegacyKatexFonts()],
  build: {
    target: 'es2023',
  },
})
