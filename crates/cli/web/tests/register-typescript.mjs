import { registerHooks } from 'node:module'

// 在执行真实模块的同时匹配 Vite 的无扩展名 TypeScript import：Node 的解析器
// 不补 .ts，这里先按原样解析，失败再补一次后缀。
const sourceRoot = new URL('../', import.meta.url).href
registerHooks({
  resolve(specifier, context, nextResolve) {
    const relative = specifier.startsWith('./') || specifier.startsWith('../')
    if (!relative || !context.parentURL?.startsWith(sourceRoot)) {
      return nextResolve(specifier, context)
    }
    try {
      return nextResolve(specifier, context)
    } catch (error) {
      if (error?.code !== 'ERR_MODULE_NOT_FOUND') {
        throw error
      }
      return nextResolve(`${specifier}.ts`, context)
    }
  },
})
