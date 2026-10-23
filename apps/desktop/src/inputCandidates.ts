import { useLayoutEffect, useState } from 'react'
import { appStore, type AppState } from './appStore'
import type { FileCandidate, SkillCatalog } from './protocol'

type CandidateQuery<Result> =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ready'; result: Result }
  | { status: 'error'; error: Error }

type QueryScope = Pick<AppState, 'selectedWorkspaceRoot' | 'selectedSessionId' | 'connection'>

export function useFileCandidates(query: string | undefined, scope: QueryScope) {
  const { selectedWorkspaceRoot, selectedSessionId, connection } = scope
  const [files, setFiles] = useState<CandidateQuery<FileCandidate[]>>({ status: 'idle' })
  const [reload, setReload] = useState(0)
  useLayoutEffect(() => {
    // 切换任务或查询时先清除旧候选，避免插入其他任务的文件。
    if (!query?.trim() || connection !== 'ready' || selectedWorkspaceRoot === null) {
      setFiles({ status: 'idle' })
      return
    }
    setFiles({ status: 'loading' })
    let active = true
    const queryTimer = setTimeout(() => {
      void appStore.transport.rpc('file.search', {
        workspaceRoot: selectedWorkspaceRoot,
        query: query.trim(),
      }).then(
        result => { if (active) setFiles({ status: 'ready', result }) },
        error => { if (active) setFiles({ status: 'error', error: error instanceof Error ? error : new Error(String(error)) }) },
      )
    }, 150)
    return () => { active = false; clearTimeout(queryTimer) }
  }, [query, selectedSessionId, selectedWorkspaceRoot, connection, reload])
  return { ...files, retry: () => setReload(value => value + 1) }
}

export function useSkillCandidates(enabled: boolean, scope: QueryScope) {
  const { selectedWorkspaceRoot, selectedSessionId, connection } = scope
  const [skills, setSkills] = useState<CandidateQuery<SkillCatalog>>({ status: 'idle' })
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
