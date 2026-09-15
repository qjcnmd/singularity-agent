import { registerHooks } from 'node:module'

// 在执行真实模块的同时匹配 Vite 的无扩展名 TypeScript import。
const sourceRoot = new URL('../', import.meta.url).href
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (context.parentURL?.startsWith(sourceRoot) && /^\.\.?\/[^.]+$/.test(specifier)) {
      return nextResolve(`${specifier}.ts`, context)
    }
    return nextResolve(specifier, context)
  },
})
