import { useLayoutEffect, useState } from 'react'
import { appStore, type AppState } from './appStore'
import type { SkillCatalog } from './protocol'

/** 识别光标前的 `/`：仅在行首或空白后、且后面不含路径分隔符时触发技能候选。 */
export function inputTrigger(text: string, caret: number): { start: number; end: number; query: string } | null {
  for (let start = caret - 1; start >= 0; start--) {
    const marker = text[start]
    if (/\s/u.test(marker)) break
    if (marker !== '/') continue
    const previous = text[start - 1]
    if (previous && !/\s/u.test(previous) || /[\/\\]/u.test(text.slice(start + 1, caret))) continue
    return { start, end: caret, query: text.slice(start + 1, caret) }
  }
  return null
}

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
