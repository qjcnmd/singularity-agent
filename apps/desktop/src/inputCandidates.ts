import { useLayoutEffect, useState } from 'react'
import { appStore, type AppState } from './appStore'
import type { SkillCatalog } from './protocol'

export function inputTrigger(text: string, caret: number): { start: number; end: number; query: string } | null {
  for (let start = caret - 1; start >= 0; start--) {
    const marker = text[start]
    if (/\s/u.test(marker)) break
    if (marker !== '/') continue
    const previous = text[start - 1]
    if (previous && !/\s/u.test(previous) || /[/\\]/u.test(text.slice(start + 1, caret))) continue
    return { start, end: caret, query: text.slice(start + 1, caret) }
  }
  return null
}

type SkillQuery =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ready'; result: SkillCatalog }
  | { status: 'error'; error: Error }

type QueryScope = Pick<AppState, 'selectedWorkspaceRoot' | 'selectedSessionId' | 'bootstrap'>

export function useSkillCandidates(enabled: boolean, scope: QueryScope) {
  const { selectedWorkspaceRoot, selectedSessionId, bootstrap } = scope
  const ready = bootstrap !== null
  const [skills, setSkills] = useState<SkillQuery>({ status: 'idle' })
  const [reload, setReload] = useState(0)
  useLayoutEffect(() => {
    if (!enabled || !ready || selectedWorkspaceRoot === null) {
      setSkills({ status: 'idle' })
      return
    }
    setSkills({ status: 'loading' })
    let active = true
    void appStore.transport.rpc('skills.list', { workspaceRoot: selectedWorkspaceRoot }).then(
      result => { if (active) setSkills({ status: 'ready', result }) },
      error => { if (active) setSkills({ status: 'error', error: error instanceof Error ? error : new Error(String(error)) }) },
    )
    return () => { active = false }
  }, [enabled, selectedSessionId, selectedWorkspaceRoot, ready, reload])
  return { ...skills, retry: () => setReload(value => value + 1) }
}
