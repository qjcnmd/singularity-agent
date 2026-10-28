import { useLayoutEffect, useState } from 'react'
import { appStore } from './appStore'
import type { ContextModule } from './protocol'

type ModuleQuery =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ready'; modules: ContextModule[] }
  | { status: 'error'; error: Error }

/** 新任务页读取当前项目的上下文胶囊；切换项目作废上一次读取，失败可重试。 */
export function useContextModules(workspaceRoot: string | null, connection: string) {
  const [query, setQuery] = useState<ModuleQuery>({ status: 'idle' })
  const [reload, setReload] = useState(0)
  useLayoutEffect(() => {
    if (workspaceRoot === null || connection !== 'ready') {
      setQuery({ status: 'idle' })
      return
    }
    setQuery({ status: 'loading' })
    let active = true
    void appStore.transport.rpc('context.preview', { workspaceRoot }).then(
      modules => { if (active) setQuery({ status: 'ready', modules }) },
      error => { if (active) setQuery({ status: 'error', error: error instanceof Error ? error : new Error(String(error)) }) },
    )
    return () => { active = false }
  }, [workspaceRoot, connection, reload])
  return { query, retry: () => setReload(value => value + 1) }
}
