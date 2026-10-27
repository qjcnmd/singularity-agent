import { useLayoutEffect, useState } from 'react'
import { appStore, type AppState } from './appStore'
import type { SkillCatalog } from './protocol'

type SkillQuery =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ready'; result: SkillCatalog }
  | { status: 'error'; error: Error }

type QueryScope = Pick<AppState, 'selectedWorkspaceRoot' | 'selectedSessionId' | 'connection'>

export function useSkillCandidates(enabled: boolean, scope: QueryScope) {
  const { selectedWorkspaceRoot, selectedSessionId, connection } = scope
  const [skills, setSkills] = useState<SkillQuery>({ status: 'idle' })
  const [reload, setReload] = useState(0)
  useLayoutEffect(() => {
    if (!enabled || connection !== 'ready' || selectedWorkspaceRoot === null) {
      setSkills({ status: 'idle' })
      return
    }
    setSkills({ status: 'loading' })
    let active = true
    void appStore.transport.rpc('skills.list', { workspaceRoot: selectedWorkspaceRoot }).then(
      result => { if (active) setSkills({ status: 'ready', result }) },
      error => { if (active) setSkills({ status: 'error', error: error instanceof Error ? error : new Error(String(error)) }) },
    )
    // 修改技能前缀只筛选当前目录；切换任务或退出技能输入才使这次读取失效。
    return () => { active = false }
  }, [enabled, selectedSessionId, selectedWorkspaceRoot, connection, reload])
  return { ...skills, retry: () => setReload(value => value + 1) }
}
