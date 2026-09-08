import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useSelectionGuard } from '../interactions'
import type { ControlSnapshot } from '../protocol'
import { workbenchStore, useWorkbenchStore, type WorkbenchState } from '../store'
import { ModelPicker } from './ModelPicker'
import { ActivityOrb } from './ActivityOrb'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { flushSync } from 'react-dom'
import { Settings, MessageSquare, Pencil, Trash2, ArrowUp, Check, X, ChevronDown } from 'lucide-react'
import { contextOccupancy } from '../contextUsage'
import { inputTrigger, type SkillCatalog } from '../inputTrigger'

export const Composer = memo(ComposerView)

function ComposerView() {
  const state = useWorkbenchStore(['drafts', 'viewportAnchors', 'actionErrors', 'pendingActions', 'bootstrap', 'connection', 'fileCandidateError', 'fileCandidates', 'fileCandidateStatus', 'selectedSessionId', 'selectedWorkspaceId', 'session', 'sessionLoad', 'theme'])
  const draft = workbenchStore.draft()
  const phase = state.session?.runtime.phase ?? 'idle'
  const hasTurns = state.session?.history.turns.some(turn => turn.turnId !== null) ?? false
  const queue = state.session?.runtime.pendingControls.filter((control) => control.channel === 'follow_up') ?? []
  const [caret, setCaret] = useState(draft.length)
  const trigger = inputTrigger(draft, caret)
  const fileQuery = trigger?.kind === 'file' ? trigger.query : undefined
  const skillQuery = trigger?.kind === 'skill' ? trigger.query : undefined
  const [skills, setSkills] = useState<SkillCatalog | null>(null)
  const [skillError, setSkillError] = useState<string | null>(null)
  const skillMenu = skillQuery !== undefined
  const suggestions = skillMenu
    ? (skills?.skills ?? []).filter(skill => skill.name.startsWith(skillQuery)).map(skill => ({ value: skill.name, description: skill.description }))
    : state.fileCandidates.map(file => ({ value: file.path, description: '任务文件' }))
  const [suggestionIndex, setSuggestionIndex] = useState(0)
  const [suggestionsOpen, setSuggestionsOpen] = useState(true)
  const [modelPickerOpen, setModelPickerOpen] = useState(false)
  const textarea = useRef<HTMLTextAreaElement>(null)
  const candidateList = useRef<HTMLDivElement>(null)
  const selectionGuard = useSelectionGuard()
  const sessionOrigin = state.selectedSessionId === null ? undefined : `session:${state.selectedSessionId}`
  const occupancy = useMemo(() => contextOccupancy(state.session, state.bootstrap?.modelCatalog), [state.session, state.bootstrap?.modelCatalog])
  useEffect(() => {
    setCaret(textarea.current?.selectionStart ?? draft.length)
  }, [draft, state.selectedSessionId, state.selectedWorkspaceId])
  useEffect(() => {
    void workbenchStore.searchFiles(fileQuery ?? '')
  }, [fileQuery, state.selectedSessionId, state.selectedWorkspaceId])
  useEffect(() => {
    if (!skillMenu || state.connection !== 'ready' || state.selectedWorkspaceId === null) return
    let active = true
    setSkills(null)
    setSkillError(null)
    void workbenchStore.listSkills().then(catalog => { if (active) setSkills(catalog) }, error => {
      if (active) setSkillError(error instanceof Error ? error.message : String(error))
    })
    return () => { active = false }
  }, [skillMenu, state.selectedSessionId, state.selectedWorkspaceId, state.connection])
  useEffect(() => setSuggestionIndex((index) => Math.min(index, Math.max(0, suggestions.length - 1))), [suggestions.length])
  useLayoutEffect(() => {
    const node = textarea.current
    if (node === null) return
    const resize = () => {
      node.style.height = 'auto'
      node.style.height = `${Math.min(node.scrollHeight, 220)}px`
    }
    resize()
    let width = node.clientWidth
    const observer = new ResizeObserver(() => {
      if (node.clientWidth === width) return
      width = node.clientWidth
      resize()
    })
    observer.observe(node)
    return () => observer.disconnect()
  }, [draft, phase, state.selectedSessionId])

  const submitPending = ['session.submit', 'session.followUp', 'session.steer'].some(method => workbenchStore.isPending(method, sessionOrigin))
  const creating = workbenchStore.isPending('session.create', `workspace:${state.selectedWorkspaceId}`)
  const canSubmit = state.selectedWorkspaceId !== null && (state.selectedSessionId === null || state.session !== null)
    && state.connection === 'ready'
    && draft.trim() !== ''
    && phase !== 'compacting'
    && phase !== 'stopping'
    && phase !== 'reserved'
    && !creating
    && !submitPending
  const blockedReason = state.connection !== 'ready' ? '连接恢复后即可发送，草稿会保留。'
      : creating ? '正在准备新任务，输入的内容会保留。'
        : state.selectedSessionId !== null && state.session === null ? state.sessionLoad.status === 'error' ? '任务读取失败，请点击上方“重试读取”。' : '正在读取任务，稍后即可发送。'
          : phase === 'stopping' ? '正在停止当前任务，结束后即可发送。'
            : phase === 'reserved' ? '正在启动任务，稍后可继续发送。'
              : phase === 'compacting' ? '上下文整理完成后即可发送，也可以先停止整理。'
                : submitPending ? '正在发送…' : null

  const insertCandidate = (text: string) => {
    if (trigger === null) return
    const insertion = `${trigger.kind === 'skill' ? '/' : '@'}${text} `
    const position = trigger.start + insertion.length
    workbenchStore.setDraft(draft.slice(0, trigger.start) + insertion + draft.slice(trigger.end))
    setCaret(position)
    requestAnimationFrame(() => { textarea.current?.focus(); textarea.current?.setSelectionRange(position, position) })
    setSuggestionsOpen(false)
    workbenchStore.clearFileCandidates()
  }

  const chooseSuggestion = (index: number) => {
    const suggestion = suggestions[index]
    if (suggestion !== undefined) insertCandidate(suggestion.value)
  }

  const showCandidateSurface = suggestionsOpen
    && trigger !== null && (skillMenu || suggestions.length > 0 || (fileQuery !== undefined && state.fileCandidateStatus !== 'idle'))

  useLayoutEffect(() => {
    candidateList.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' })
  }, [suggestionIndex, suggestions.length, showCandidateSurface])

  return (
    <section className="composer-region" aria-label="任务输入区">


      <AnimatePresence initial={false}>{queue.length > 0 && <FollowUpQueue key={state.selectedSessionId} controls={queue} state={state} />}</AnimatePresence>
      {showCandidateSurface && (
        <div ref={candidateList} className="composer-candidates" id="composer-suggestions" role="listbox" aria-label="输入建议">
          {suggestions.map((candidate, candidateIndex) => {
            const index = candidateIndex
            return (
              <button
                type="button"
                role="option"
                aria-selected={suggestionIndex === index}
                id={`composer-suggestion-${index}`}
                key={candidate.value}
                onMouseDown={event => event.preventDefault()}
                {...selectionGuard(() => insertCandidate(candidate.value))}
              >
                <strong>{skillMenu ? '/' : '@'}{candidate.value}</strong><span>{candidate.description}</span>
              </button>
            )
          })}
          {skillMenu && skills === null && skillError === null && <p className="candidate-message">正在读取 Skills…</p>}
          {skillMenu && skills !== null && suggestions.length === 0 && <p className="candidate-message">{skills.skills.length === 0 ? '没有可用的 Skills' : '没有匹配的 Skills'}</p>}
          {skillMenu && skillError !== null && <p className="candidate-message candidate-error" role="alert">{skillError}</p>}
          {skillMenu && skills?.diagnostics.map(message => <p className="candidate-message candidate-error" role="alert" key={message}>{message}</p>)}
          {fileQuery !== undefined && state.fileCandidateStatus === 'loading' && <p className="candidate-message">正在查找任务文件…</p>}
          {fileQuery !== undefined && state.fileCandidateStatus === 'empty' && <p className="candidate-message">没有匹配的文件</p>}
          {fileQuery !== undefined && state.fileCandidateStatus === 'error' && state.fileCandidateError !== null && (
            <div className="candidate-message candidate-error" role="alert">
              <strong>{state.fileCandidateError.message}</strong><span>{state.fileCandidateError.recovery}</span>
            </div>
          )}
        </div>
      )}
      <div className={`composer-card phase-${phase}`}>
        <textarea
          ref={textarea}
          readOnly={state.selectedWorkspaceId === null}
          onClick={() => { if (state.selectedWorkspaceId === null) workbenchStore.openDirectoryPicker() }}
          value={draft}
          onSelect={event => { setCaret(event.currentTarget.selectionStart) }}
          onChange={(event) => {
            const value = event.target.value
            workbenchStore.setDraft(value)
            setCaret(event.target.selectionStart)
            setSuggestionsOpen(true)
            setSuggestionIndex(0)
          }}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing) return
            if (state.selectedWorkspaceId === null && (event.key === 'Enter' || event.key === ' ')) { event.preventDefault(); workbenchStore.openDirectoryPicker(); return }
            if (showCandidateSurface && suggestions.length > 0 && (event.key === 'ArrowDown' || event.key === 'ArrowUp')) {
              event.preventDefault()
              const direction = event.key === 'ArrowDown' ? 1 : -1
              setSuggestionIndex((index) => (index + direction + suggestions.length) % suggestions.length)
              return
            }
            if (showCandidateSurface && suggestions.length > 0 && event.key === 'Tab') {
              event.preventDefault()
              chooseSuggestion(suggestionIndex)
              return
            }
            if (suggestionsOpen && showCandidateSurface && event.key === 'Escape') {
              event.preventDefault()
              setSuggestionsOpen(false)
              workbenchStore.clearFileCandidates()
              return
            }
            if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault()
              if (event.repeat) return
              if (showCandidateSurface && suggestions.length > 0) chooseSuggestion(suggestionIndex)
              else if (phase === 'running' && draft.trim() === '' && (event.ctrlKey || event.metaKey)) void workbenchStore.sendQueuedNow()
              else if (canSubmit) {
                void workbenchStore.submitDraft(event.ctrlKey || event.metaKey ? 'steer' : 'follow_up')
              }
            }
          }}
          rows={1}
          aria-label="任务说明"
          aria-controls={showCandidateSurface ? 'composer-suggestions' : undefined}
          aria-activedescendant={suggestionsOpen && suggestions.length > 0 ? `composer-suggestion-${suggestionIndex}` : undefined}
        />
        <div className="composer-toolbar">
          <div className="composer-context">
            <ComposerTools key={state.selectedSessionId ?? state.selectedWorkspaceId}
              theme={state.theme} occupancy={occupancy} started={state.session === null ? state.selectedSessionId === null ? false : undefined : hasTurns || phase !== 'idle'}
              compactDisabled={state.session === null || !hasTurns || state.connection !== 'ready' || phase !== 'idle' || workbenchStore.isPending('session.compact', sessionOrigin)} />

          </div>
          <div className="composer-actions">
            <ModelPicker state={state} open={modelPickerOpen} onOpenChange={setModelPickerOpen} />
            {(phase === 'running' || phase === 'stopping' || phase === 'compacting') && (
              <button
                type="button"
                className="stop-button"
                {...selectionGuard(() => { void workbenchStore.stopActive() })}
                disabled={phase === 'stopping' || workbenchStore.isPending('session.abort', sessionOrigin)}
                aria-label={phase === 'stopping' ? '正在停止' : phase === 'compacting' ? '停止压缩' : '停止当前任务'}
                title={phase === 'stopping' ? '正在停止' : phase === 'compacting' ? '停止压缩' : '停止'}
              >
                <ActivityOrb fast />
              </button>
            )}
            {(phase === 'idle' || phase === 'reserved') && <button
              type="button"
              className="submit-button"
              disabled={!canSubmit}
              aria-label="发送消息"
              title={blockedReason ?? '发送'}
              {...selectionGuard(() => { void workbenchStore.submitDraft() })}
            >
              <ActivityOrb />
            </button>}
          </div>
        </div>
      </div>
    </section>
  )
}

function ContextRing({ percent = 0 }: { percent?: number }) {
  return <svg className="context-ring" viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="7" /><circle cx="10" cy="10" r="7" pathLength="100" strokeDasharray={`${percent} 100`} /></svg>
}

function ComposerTools({ compactDisabled, theme, occupancy, started }: { compactDisabled: boolean; theme: WorkbenchState['theme']; occupancy: { used: number; capacity: number; percent: number } | null; started: boolean | undefined }) {
  const [expanded, setExpanded] = useState(false)
  const [confirming, setConfirming] = useState(false)
  const changeExpanded = useCallback((next: boolean) => {
    setConfirming(false)
    setExpanded(next)
  }, [])
  const hasStarted = useRef(started)
  useEffect(() => {
    if (started === undefined) return
    if (started && hasStarted.current === false) changeExpanded(true)
    hasStarted.current = hasStarted.current || started
  }, [started, changeExpanded])
  const compactButton = useRef<HTMLButtonElement>(null)
  const reducedMotion = useReducedMotion()
  const guard = useSelectionGuard()

  useEffect(() => { if (compactDisabled) setConfirming(false) }, [compactDisabled])
  useEffect(() => {
    if (!confirming) return
    const cancelOutside = (event: Event) => {
      if (!compactButton.current?.contains(event.target as Node)) setConfirming(false)
    }
    document.addEventListener('pointerdown', cancelOutside, true)
    document.addEventListener('click', cancelOutside, true)
    return () => {
      document.removeEventListener('pointerdown', cancelOutside, true)
      document.removeEventListener('click', cancelOutside, true)
    }
  }, [confirming])

  return <aside className="composer-tools" aria-label="任务工具" onKeyDown={event => {
    if (event.key === 'Escape') changeExpanded(false)
  }}>
    <button type="button" className="composer-tools-toggle" aria-label={expanded ? '收起任务工具' : '展开任务工具'} aria-expanded={expanded}
      {...guard(() => changeExpanded(!expanded))}>
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" aria-hidden="true"><path d="M5 12h14" /><path className={expanded ? 'tools-plus-stem is-hidden' : 'tools-plus-stem'} d="M12 5v14" /></svg>
    </button>
    <motion.div className="composer-tools-reveal" initial={false} animate={{ width: expanded ? 'auto' : 0, opacity: expanded ? 1 : 0 }}
      transition={{ duration: reducedMotion ? 0 : 0.24, ease: [0.2, 0.8, 0.2, 1] }} inert={!expanded} aria-hidden={!expanded}>
      <div className="composer-tools-items">
        <button ref={compactButton} type="button" className="compact-button" aria-disabled={compactDisabled}
          aria-label={confirming ? '确认压缩上下文' : '压缩上下文'}
          {...guard(() => {
            if (compactDisabled) return
            if (confirming) { setConfirming(false); void workbenchStore.compact() }
            else setConfirming(true)
          })}>
          <span className="compact-label" aria-hidden="true">{confirming ? '确认' : <ContextRing percent={occupancy?.percent} />}</span>
          {occupancy && <span className="context-tooltip" role="tooltip"><span>上下文窗口：</span><span>{occupancy.percent}% 已用</span><strong>已用 {compactTokens(occupancy.used)} 标记，共 {compactTokens(occupancy.capacity)}</strong></span>}
        </button>
        <button type="button" className="theme-toggle" aria-label={theme === 'light' ? '切换深色模式' : '切换浅色模式'} title={theme === 'light' ? '切换深色模式' : '切换浅色模式'} onClick={() => {
          const update = () => flushSync(() => workbenchStore.setTheme(theme === 'light' ? 'dark' : 'light'))
          if (!reducedMotion && document.startViewTransition) document.startViewTransition(update)
          else update()
        }}>
          <svg key={theme} width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{theme === 'light' ? <><circle cx="12" cy="12" r="4" /><path d="M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M5 19l1.5-1.5m11-11L19 5" /></> : <path d="M20.5 14a8.5 8.5 0 0 1-10.5-10.5A8.5 8.5 0 1 0 20.5 14Z" />}</svg>
        </button>
        <button type="button" className="composer-settings" aria-label="设置" title="设置" {...guard(() => workbenchStore.setSettingsOpen(true))}>
          <Settings size={17} strokeWidth={1.6} aria-hidden="true" />
        </button>
      </div>
    </motion.div>
  </aside>
}

function compactTokens(value: number): string {
  return value < 1000 ? String(value) : `${Number((value / 1000).toFixed(1))}k`
}

const queueTransition = { duration: 0.24, ease: [0.2, 0.8, 0.2, 1] as const }

function FollowUpQueue({ controls, state }: { controls: ControlSnapshot[]; state: WorkbenchState }) {
  const reducedMotion = useReducedMotion()
  const transition = { ...queueTransition, duration: reducedMotion ? 0 : queueTransition.duration }
  const [expanded, setExpanded] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  const visible = expanded || editingId !== null ? controls : controls.slice(0, 1)
  return <motion.div className="follow-up-queue-motion" initial={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} animate={{ height: 'auto', opacity: 1, y: 0, marginBottom: -8 }} exit={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} transition={transition}><div className="follow-up-queue" aria-label="排队消息">
    {controls.length > 1 && <button className="queue-toggle" type="button" aria-expanded={expanded || editingId !== null} onClick={() => setExpanded(!expanded)}>
      <ChevronDown size={14} />{controls.length} 条排队消息
    </button>}
    <AnimatePresence initial={false}>{visible.map(control => <motion.div key={control.controlId} initial={{ height: 0, opacity: 0, y: 10 }} animate={{ height: 'auto', opacity: 1, y: 0 }} exit={{ height: 0, opacity: 0, y: 10 }} transition={transition} style={{ overflow: 'hidden' }}><QueueRow control={control} state={state}
      editing={editingId === control.controlId} onEdit={value => setEditingId(value ? control.controlId : null)} /></motion.div>)}</AnimatePresence>
  </div></motion.div>
}

function QueueRow({ control, state, editing, onEdit }: { control: ControlSnapshot; state: WorkbenchState; editing: boolean; onEdit: (value: boolean) => void }) {
  const [text, setText] = useState(control.text ?? '')
  const selectionGuard = useSelectionGuard()
  const origin = `control:${state.selectedSessionId}:${control.controlId}`
  const pending = ['session.queueReplace', 'session.queueSendNow', 'session.queueWithdraw']
    .some(method => workbenchStore.isPending(method, origin))
  const error = state.actionErrors[origin]
  const save = async () => {
    if (pending || text.trim() === '') return
    if (await workbenchStore.replace(control.controlId, text)) onEdit(false)
  }
  const cancel = () => { setText(control.text ?? ''); onEdit(false) }
  return <div className="queue-row">
    <MessageSquare size={16} aria-hidden="true" />
    {editing ? <textarea autoFocus value={text} onChange={event => setText(event.target.value)} aria-label="编辑排队消息"
      onKeyDown={event => {
        if (event.key === 'Escape') { event.preventDefault(); cancel() }
        if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); if (!event.repeat) void save() }
      }} /> : <span className="queue-text">{control.text}</span>}
    <span className="queue-actions">
      {editing ? <>
        <button type="button" aria-label="保存消息" title="保存" disabled={pending || text.trim() === ''} {...selectionGuard(() => { void save() })}><Check size={17} /></button>
        <button type="button" aria-label="取消编辑" title="取消编辑" disabled={pending} {...selectionGuard(cancel)}><X size={17} /></button>
      </> : <>
        <button type="button" aria-label="编辑消息" title="编辑" disabled={pending} {...selectionGuard(() => { setText(control.text ?? ''); onEdit(true) })}><Pencil size={17} /></button>
        <button type="button" aria-label="删除排队消息" title="删除" disabled={pending} {...selectionGuard(() => { void workbenchStore.withdraw(control.controlId) })}><Trash2 size={17} /></button>
        <button type="button" aria-label="立即发送排队消息" title="立即发送" disabled={pending} {...selectionGuard(() => { void workbenchStore.sendNow(control.controlId) })}><ArrowUp size={19} /></button>
      </>}
    </span>
    {error !== undefined && <div className="queue-error" role="alert"><strong>{error.message}</strong><span>{error.recovery}</span></div>}
  </div>
}
