import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { navigateList, useSelectionGuard, useDismissOnOutside } from '../interactions'
import { RpcFailure } from '../rpcClient'
import type { FileCandidate, ControlSnapshot, SkillCatalog, SessionModelUsage } from '../protocol'
import { actionOrigin, appStore, useAppStore, pendingKey, type AppState } from '../appStore'
import { ModelPicker } from './ModelPicker'
import { ActivityOrb } from './ActivityOrb'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { flushSync } from 'react-dom'
import { Settings, MessageSquare, Pencil, Trash2, ArrowUp, Check, X, ChevronDown } from 'lucide-react'
import { formatTokenCount } from '../copy'
import { contextOccupancy } from '../contextUsage'
import { cacheHitPercent, generationRate, sessionUsage, totalTokens } from '../sessionUsage'
import { inputTrigger } from '../inputTrigger'
import { disclosureTransition } from '../motion'

export const Composer = memo(ComposerView)

function ComposerView() {
  const state = useAppStore(['drafts', 'actionErrors', 'pendingActions', 'bootstrap', 'connection', 'selectedSessionId', 'selectedWorkspaceId', 'session', 'sessionLoad', 'theme'])
  const draft = appStore.draft()
  const phase = state.session?.runtime.phase ?? 'idle'
  const busy = phase === 'running' || phase === 'stopping' || phase === 'compacting'
  const hasTurns = state.session?.facts.history.some(turn => turn.id !== null) ?? false
  // 待执行集合由会话快照一次决定：接受来源（steer / follow-up）只作展示信息，
  // 界面对两者提供同一套撤回、编辑与立即发送操作。
  const queue = state.session?.runtime.pendingControls ?? []
  const [caret, setCaret] = useState(draft.length)
  const trigger = inputTrigger(draft, caret)
  const fileQuery = trigger?.kind === 'file' ? trigger.query : undefined
  const skillQuery = trigger?.kind === 'skill' ? trigger.query : undefined
  const [skills, setSkills] = useState<SkillCatalog | null>(null)
  const [skillError, setSkillError] = useState<string | null>(null)
  const [files, setFiles] = useState<FileCandidate[] | null>(null)
  const [fileError, setFileError] = useState<Error | null>(null)
  const fileStatus = !fileQuery?.trim() || state.connection !== 'ready' || state.selectedWorkspaceId === null ? 'idle'
    : fileError ? 'error' : files === null ? 'loading' : files.length ? 'ready' : 'empty'
  const skillMenu = skillQuery !== undefined
  const suggestions = skillMenu
    ? (skills?.skills ?? []).filter(skill => skill.name.startsWith(skillQuery)).map(skill => ({ value: skill.name, description: skill.description }))
    : (files ?? []).map(file => ({ value: file.path, description: '任务文件' }))
  const [suggestionIndex, setSuggestionIndex] = useState(0)
  const [suggestionsOpen, setSuggestionsOpen] = useState(true)
  const [modelPickerOpen, setModelPickerOpen] = useState(false)
  const textarea = useRef<HTMLTextAreaElement>(null)
  const candidateList = useRef<HTMLDivElement>(null)
  const selectionGuard = useSelectionGuard()
  const sessionOrigin = state.selectedSessionId === null ? undefined : actionOrigin.session(state.selectedSessionId)
  const occupancy = useMemo(() => contextOccupancy(state.session, state.bootstrap?.modelCatalog), [state.session, state.bootstrap?.modelCatalog])
  const usage = useMemo(() => sessionUsage(state.session), [state.session])
  useEffect(() => {
    setCaret(textarea.current?.selectionStart ?? draft.length)
  }, [draft, state.selectedSessionId, state.selectedWorkspaceId])
  useLayoutEffect(() => {
    // 在变化的 task 或 token 变为可交互前清除上一次查询。
    setFiles(null)
    setFileError(null)
    if (!fileQuery?.trim() || state.connection !== 'ready' || state.selectedWorkspaceId === null) return
    let active = true
    // 候选上限属于这次文件补全交互，留在调用处；查询复用 Store 持有的同一条连接。
    void appStore.transport.rpc('file.search', {
      workspaceId: state.selectedWorkspaceId,
      sessionId: state.selectedSessionId,
      query: fileQuery.trim(),
      limit: 12,
    })
      .then(files => { if (active) setFiles(files) }, error => { if (active) setFileError(error instanceof Error ? error : new Error(String(error))) })
    return () => { active = false }
  }, [fileQuery, state.selectedSessionId, state.selectedWorkspaceId, state.connection])
  useLayoutEffect(() => {
    // 在变化的 workspace 或 task 变为可交互前清除上一次查询。
    setSkills(null)
    setSkillError(null)
    if (!skillMenu || state.connection !== 'ready' || state.selectedWorkspaceId === null) return
    let active = true
    void appStore.transport.rpc('skills.list', { workspaceId: state.selectedWorkspaceId, sessionId: state.selectedSessionId }).then(catalog => { if (active) setSkills(catalog) }, error => {
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

  const { canSubmit, blockedReason } = appStore.submissionState()

  const insertCandidate = (text: string) => {
    if (trigger === null) return
    const insertion = `${trigger.kind === 'skill' ? '/' : '@'}${text} `
    const position = trigger.start + insertion.length
    appStore.setDraft(draft.slice(0, trigger.start) + insertion + draft.slice(trigger.end))
    setCaret(position)
    requestAnimationFrame(() => { textarea.current?.focus(); textarea.current?.setSelectionRange(position, position) })
    setSuggestionsOpen(false)
  }

  const chooseSuggestion = (index: number) => {
    const suggestion = suggestions[index]
    if (suggestion !== undefined) insertCandidate(suggestion.value)
  }

  const showCandidateSurface = suggestionsOpen
    && trigger !== null && (skillMenu || suggestions.length > 0 || (fileQuery !== undefined && fileStatus !== 'idle'))

  useLayoutEffect(() => {
    candidateList.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' })
  }, [suggestionIndex, suggestions.length, showCandidateSurface])

  return (
    <section className="composer-region" aria-label="任务输入区">


      <AnimatePresence initial={false}>{queue.length > 0 && <QueuedInputs key={state.selectedSessionId} controls={queue} state={state} />}</AnimatePresence>
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
          {fileQuery !== undefined && fileStatus === 'loading' && <p className="candidate-message">正在查找任务文件…</p>}
          {fileQuery !== undefined && fileStatus === 'empty' && <p className="candidate-message">没有匹配的文件</p>}
          {fileQuery !== undefined && fileStatus === 'error' && fileError !== null && (
            <div className="candidate-message candidate-error" role="alert">
              <strong>{fileError.message}</strong><span>{fileError instanceof RpcFailure ? fileError.recovery : '请重试文件查询。'}</span>
            </div>
          )}
        </div>
      )}
      <div className={`composer-card phase-${phase}`}>
        <textarea
          ref={textarea}
          readOnly={state.selectedWorkspaceId === null}
          onClick={() => { if (state.selectedWorkspaceId === null) appStore.openDirectoryPicker() }}
          value={draft}
          onSelect={event => { setCaret(event.currentTarget.selectionStart) }}
          onChange={(event) => {
            const value = event.target.value
            appStore.setDraft(value)
            setCaret(event.target.selectionStart)
            setSuggestionsOpen(true)
            setSuggestionIndex(0)
          }}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing) return
            if (state.selectedWorkspaceId === null && (event.key === 'Enter' || event.key === ' ')) { event.preventDefault(); appStore.openDirectoryPicker(); return }
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
            if (showCandidateSurface && event.key === 'Escape') {
              event.preventDefault()
              setSuggestionsOpen(false)
              return
            }
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault()
              if (event.repeat) return
              if (showCandidateSurface && suggestions.length > 0) chooseSuggestion(suggestionIndex)
              else if (phase === 'running' && draft.trim() === '' && (event.ctrlKey || event.metaKey)) void appStore.sendQueuedNow()
              else if (canSubmit) {
                void appStore.submitDraft(event.ctrlKey || event.metaKey ? 'steer' : 'follow_up')
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
              compactDisabled={state.session === null || !hasTurns || state.connection !== 'ready' || phase !== 'idle' || state.pendingActions.has(pendingKey('session.compact', sessionOrigin))} />

          </div>
          <div className="composer-actions">
            <ModelPicker state={state} open={modelPickerOpen} onOpenChange={setModelPickerOpen} />
            {busy && (
              <button
                type="button"
                className="stop-button"
                {...selectionGuard(() => { void appStore.stopActive() })}
                disabled={phase === 'stopping' || state.pendingActions.has(pendingKey('session.abort', sessionOrigin))}
                aria-label={phase === 'stopping' ? '正在停止' : phase === 'compacting' ? '停止压缩' : '停止当前任务'}
                title={phase === 'stopping' ? '正在停止' : phase === 'compacting' ? '停止压缩' : '停止'}
              >
                <ActivityOrb theme={state.theme} fast />
              </button>
            )}
            {!busy && <button
              type="button"
              className="submit-button"
              disabled={!canSubmit}
              aria-label="发送消息"
              title={blockedReason ?? '发送'}
              {...selectionGuard(() => { void appStore.submitDraft() })}
            >
              <ActivityOrb theme={state.theme} />
            </button>}
          </div>
        </div>
      </div>
      {usage !== null && <ComposerStats usage={usage} />}
    </section>
  )
}

function ContextRing({ percent = 0 }: { percent?: number }) {
  return <svg className="context-ring" viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="7" /><circle cx="10" cy="10" r="7" pathLength="100" strokeDasharray={`${percent} 100`} /></svg>
}

function ComposerTools({ compactDisabled, theme, occupancy, started }: { compactDisabled: boolean; theme: AppState['theme']; occupancy: { used: number; capacity: number; percent: number } | null; started: boolean | undefined }) {
  const [expanded, setExpanded] = useState(false)
  const [confirming, setConfirming] = useState(false)
  const [contextOpen, setContextOpen] = useState(false)
  const changeExpanded = useCallback((next: boolean) => {
    setConfirming(false)
    setContextOpen(false)
    setExpanded(next)
  }, [])
  const hasStarted = useRef(started)
  useEffect(() => {
    if (started === undefined) return
    if (started && hasStarted.current === false) changeExpanded(true)
    hasStarted.current = hasStarted.current || started
  }, [started, changeExpanded])
  const toolsRoot = useRef<HTMLElement>(null)
  const toggleButton = useRef<HTMLButtonElement>(null)
  const compactButton = useRef<HTMLButtonElement>(null)
  const contextButton = useRef<HTMLButtonElement>(null)
  const reducedMotion = useReducedMotion()
  const guard = useSelectionGuard()

  useEffect(() => { if (compactDisabled) setConfirming(false) }, [compactDisabled])
  useDismissOnOutside(compactButton, confirming, () => setConfirming(false), { captureClick: true })

  useEffect(() => {
    if (!expanded) return
    if (document.activeElement === toggleButton.current) compactButton.current?.focus({ preventScroll: true })
  }, [expanded])
  useDismissOnOutside(toolsRoot, expanded, () => changeExpanded(false))

  return <aside ref={toolsRoot} className="composer-tools" aria-label="任务工具" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) changeExpanded(false)
  }} onKeyDown={event => {
    if (event.key === 'Escape') {
      event.preventDefault()
      event.stopPropagation()
      if (contextOpen) { setContextOpen(false); contextButton.current?.focus({ preventScroll: true }) }
      else { changeExpanded(false); toggleButton.current?.focus({ preventScroll: true }) }
    } else if (expanded && navigateList(event.key, [...event.currentTarget.querySelectorAll<HTMLButtonElement>('.composer-tools-item button')])) {
      event.preventDefault()
    }
  }}>
    <div className="t-morph" data-open={expanded}>
      <button ref={toggleButton} type="button" className="t-morph-plus" aria-label="展开任务工具" aria-expanded={expanded}
        aria-controls="composer-tools-menu" tabIndex={expanded ? -1 : 0} aria-hidden={expanded}
        {...guard(() => changeExpanded(true))}>
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" aria-hidden="true"><path d="M5 12h14M12 5v14" /></svg>
      </button>
      <div className="t-morph-menu" id="composer-tools-menu" inert={!expanded} aria-hidden={!expanded}>
        <div className="composer-tools-item">
          <button ref={contextButton} type="button" className="composer-tools-icon context-usage-toggle" aria-label="查看上下文用量" aria-expanded={contextOpen} aria-controls="composer-context-usage" onClick={() => setContextOpen(value => !value)}><ContextRing percent={occupancy?.percent} /></button>
          <button ref={compactButton} type="button" aria-disabled={compactDisabled}
            aria-label={confirming ? '确认压缩上下文' : '压缩上下文'}
            {...guard(() => {
              if (compactDisabled) return
              if (confirming) { changeExpanded(false); toggleButton.current?.focus({ preventScroll: true }); void appStore.compact() }
              else setConfirming(true)
            })}>{confirming ? '确认压缩上下文' : '上下文压缩'}</button>
        </div>
        <div className="composer-tools-item">
          <span className="composer-tools-icon theme-icon" aria-hidden="true" onPointerDown={event => event.preventDefault()}>
            <svg key={theme} width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">{theme === 'light' ? <path d="M20.5 14a8.5 8.5 0 0 1-10.5-10.5A8.5 8.5 0 1 0 20.5 14Z" /> : <><circle cx="12" cy="12" r="4" /><path d="M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M5 19l1.5-1.5m11-11L19 5" /></>}</svg>
          </span>
          <button type="button" aria-label={theme === 'light' ? '切换深色模式' : '切换浅色模式'} onClick={() => {
            const update = () => flushSync(() => appStore.setTheme(theme === 'light' ? 'dark' : 'light'))
            if (!reducedMotion && document.startViewTransition) document.startViewTransition(update)
            else update()
          }}>{theme === 'light' ? '深色模式' : '浅色模式'}</button>
        </div>
        <div className="composer-tools-item">
          <span className="composer-tools-icon" aria-hidden="true" onPointerDown={event => event.preventDefault()}><Settings size={18} strokeWidth={1.6} /></span>
          <button type="button" {...guard(() => {
            changeExpanded(false)
            toggleButton.current?.focus({ preventScroll: true })
            appStore.setSettingsOpen(true)
          })}>设置</button>
        </div>
      </div>
    </div>
    {expanded && <span className="context-tooltip" data-open={contextOpen} id="composer-context-usage" role="tooltip">{occupancy ? <><span>上下文窗口：</span><span>{occupancy.percent}% 已用</span><strong>已用 {formatTokenCount(occupancy.used)} token，共 {formatTokenCount(occupancy.capacity)}</strong></> : <span>暂无上下文用量</span>}</span>}
  </aside>
}

/**
 * 输入框下方的用量统计条：TPS、累计 token 与缓存命中率三枚读数（数值不带前缀，
 * 口径由 sessionUsage 单点定义）。没有请求报告 usage 时整条不渲染；有请求未报告
 * 用量时合计带 ≥，悬停说明给出下界与输入、输出、耗时的明细。
 */
function ComposerStats({ usage }: { usage: SessionModelUsage }) {
  const hit = cacheHitPercent(usage)
  const rate = generationRate(usage)
  const detail = [
    `输入 ${usage.inputTokens.toLocaleString()}（缓存 ${usage.cachedInputTokens.toLocaleString()}）`,
    `输出 ${usage.outputTokens.toLocaleString()}`,
    `请求耗时 ${(usage.generationMs / 1000).toFixed(1)} 秒`,
  ].join(' · ')
  return <div className="composer-stats" title={`${detail}${usage.usageComplete ? '' : '\n有请求未报告用量，以上为下界。'}`}>
    {rate !== null && <span className="composer-stat">{rate.toFixed(1)} TPS</span>}
    <span className="composer-stat">{usage.usageComplete ? '' : '≥'}{formatTokenCount(totalTokens(usage))} token</span>
    {hit !== null && <span className="composer-stat">缓存命中率 {hit.toFixed(1)}%</span>}
  </div>
}


/** 队列行只声明自己读取的字段：Composer 按同一份清单订阅。 */
type QueueState = Pick<AppState, 'selectedSessionId' | 'actionErrors' | 'pendingActions'>

function QueuedInputs({ controls, state }: { controls: ControlSnapshot[]; state: QueueState }) {
  const reducedMotion = useReducedMotion()
  // 队列的进出场与 disclosure 共用同一组时序，避免同为展开却快慢不一。
  const transition = disclosureTransition(true, reducedMotion)
  const [expanded, setExpanded] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  // 被编辑项可能已被后台消费或撤回：只有它仍在队列里才算正在编辑。
  const editing = editingId !== null && controls.some(control => control.controlId === editingId)
  const visible = expanded || editing ? controls : controls.slice(0, 1)
  return <motion.div className="queued-inputs-motion" initial={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} animate={{ height: 'auto', opacity: 1, y: 0, marginBottom: -8 }} exit={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} transition={transition}><div className="queued-inputs" aria-label="排队消息">
    {controls.length > 1 && <button className="queue-toggle" type="button" aria-expanded={expanded || editing} onClick={() => setExpanded(!expanded)}>
      <ChevronDown size={14} />{controls.length} 条排队消息
    </button>}
    <AnimatePresence initial={false}>{visible.map(control => <motion.div key={control.controlId} initial={{ height: 0, opacity: 0, y: 10 }} animate={{ height: 'auto', opacity: 1, y: 0 }} exit={{ height: 0, opacity: 0, y: 10 }} transition={transition} style={{ overflow: 'hidden' }}><QueueRow control={control} state={state}
      editing={editingId === control.controlId} onEdit={value => setEditingId(current =>
        // 关闭编辑只作用于发起操作的那一行：保存是逐行异步的，A 的晚到回调
        // 不能关掉期间已打开的 B。
        value ? control.controlId : current === control.controlId ? null : current
      )} /></motion.div>)}</AnimatePresence>
  </div></motion.div>
}

function QueueRow({ control, state, editing, onEdit }: { control: ControlSnapshot; state: QueueState; editing: boolean; onEdit: (value: boolean) => void }) {
  const [text, setText] = useState(control.text)
  const selectionGuard = useSelectionGuard()
  const origin = actionOrigin.control(state.selectedSessionId, control.controlId)
  const pending = ['session.queueReplace', 'session.queueSendNow', 'session.queueWithdraw']
    .some(method => state.pendingActions.has(pendingKey(method, origin)))
  const error = state.actionErrors[origin]
  const save = async () => {
    if (pending || text.trim() === '') return
    if (await appStore.replace(control.controlId, text)) onEdit(false)
  }
  const cancel = () => { setText(control.text); onEdit(false) }
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
        <button type="button" aria-label="编辑消息" title="编辑" disabled={pending} {...selectionGuard(() => { setText(control.text); onEdit(true) })}><Pencil size={17} /></button>
        <button type="button" aria-label="删除排队消息" title="删除" disabled={pending} {...selectionGuard(() => { void appStore.withdraw(control.controlId) })}><Trash2 size={17} /></button>
        <button type="button" aria-label="立即发送排队消息" title="立即发送" disabled={pending} {...selectionGuard(() => { void appStore.sendNow(control.controlId) })}><ArrowUp size={19} /></button>
      </>}
    </span>
    {error !== undefined && <div className="queue-error" role="alert"><strong>{error.message}</strong><span>{error.recovery}</span></div>}
  </div>
}
