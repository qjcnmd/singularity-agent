import { registerHooks } from 'node:module'

// Match Vite's extensionless TypeScript imports while executing the real modules.
const sourceRoot = new URL('../src/', import.meta.url).href
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (context.parentURL?.startsWith(sourceRoot) && /^\.\.?\/[^.]+$/.test(specifier)) {
      return nextResolve(`${specifier}.ts`, context)
    }
    return nextResolve(specifier, context)
  },
})
