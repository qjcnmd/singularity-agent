import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { navigateList, useSelectionGuard, useDismissOnOutside } from '../interactions'
import { RpcFailure } from '../rpcClient'
import type { FileCandidate, SkillCatalog, SessionModelUsage } from '../protocol'
import { actionOrigin, appStore, useAppStore, pendingKey, type AppState } from '../appStore'
import { ModelPicker } from './ModelPicker'
import { WorkspacePicker } from './WorkspacePicker'
import { PickerSurface } from './PickerSurface'
import { ActivityOrb } from './ActivityOrb'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { flushSync } from 'react-dom'
import { ImagePlus, Settings } from 'lucide-react'
import { formatTokenCount } from '../copy'
import { contextOccupancy } from '../contextUsage'
import { cacheHitPercent, firstTokenLatency, generationRate, sessionUsage } from '../sessionUsage'
import { inputTrigger } from '../inputTrigger'
import { disclosureTransition } from '../motion'
import { QuestionPanel } from './QuestionPanel'
import { QueuedInputs } from './QueuedInputs'
import { DraftImages, useImageInput } from './Images'

export const Composer = memo(ComposerView)

function ComposerView({ centered }: { centered: boolean }) {
  const reducedMotion = useReducedMotion()
  const state = useAppStore(['drafts', 'actionErrors', 'pendingActions', 'bootstrap', 'connection', 'selectedSessionId', 'selectedWorkspaceId', 'session', 'sessionLoad', 'theme', 'workspaceAppearance'])
  const draft = appStore.draft()
  const imageDraft = appStore.inputDraft().images
  const sessionId = state.selectedSessionId
  const imageInput = useImageInput(images => { if (sessionId !== null) appStore.setImages(sessionId, images, true) }, sessionId === null || state.drafts === null)
  const phase = state.session?.runtime.phase ?? 'idle'
  const busy = phase === 'running' || phase === 'stopping' || phase === 'compacting'
  const hasTurns = state.session?.facts.history.some(turn => turn.id !== null) ?? false
  // 数组顺序就是后端的待执行顺序。
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
  const textarea = useRef<HTMLTextAreaElement>(null)
  const candidateAnchor = useRef<HTMLDivElement>(null)
  const candidateList = useRef<HTMLDivElement>(null)
  const selectionGuard = useSelectionGuard()
  const sessionOrigin = actionOrigin.session(sessionId)
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
    const workspaceId = state.selectedWorkspaceId
    let active = true
    // 候选上限属于这次文件补全交互，留在调用处；查询复用 Store 持有的同一条连接。
    const queryTimer = setTimeout(() => {
      void appStore.transport.rpc('file.search', {
        workspaceId,
        query: fileQuery.trim(),
        limit: 12,
      }).then(
        files => { if (active) setFiles(files) },
        error => { if (active) setFileError(error instanceof Error ? error : new Error(String(error))) },
      )
    }, 150)
    return () => { active = false; clearTimeout(queryTimer) }
  }, [fileQuery, state.selectedSessionId, state.selectedWorkspaceId, state.connection])
  useLayoutEffect(() => {
    // 在变化的 workspace 或 task 变为可交互前清除上一次查询。
    setSkills(null)
    setSkillError(null)
    if (!skillMenu || state.connection !== 'ready' || state.selectedWorkspaceId === null) return
    let active = true
    void appStore.transport.rpc('skills.list', { workspaceId: state.selectedWorkspaceId }).then(catalog => { if (active) setSkills(catalog) }, error => {
      if (active) setSkillError(error instanceof Error ? error.message : String(error))
    })
    return () => { active = false }
  }, [skillMenu, state.selectedSessionId, state.selectedWorkspaceId, state.connection])
  useEffect(() => setSuggestionIndex((index) => Math.min(index, Math.max(0, suggestions.length - 1))), [suggestions.length])
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
    const anchor = candidateAnchor.current
    const list = candidateList.current
    if (!showCandidateSurface || !anchor || !list) return
    const main = anchor.closest('.app-main')!
    const panelToggle = main.parentElement!.querySelector('.trajectory-toggle')!
    // 浮层避开顶部按钮，只使用输入框上方的空间；输入高度和视口变化都重新量。
    const measure = () => {
      const available = anchor.getBoundingClientRect().top - panelToggle.getBoundingClientRect().bottom - 8
      list.style.maxHeight = `${Math.max(0, Math.min(240, available))}px`
    }
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(anchor.parentElement!)
    window.addEventListener('resize', measure)
    return () => { observer.disconnect(); window.removeEventListener('resize', measure) }
  }, [showCandidateSurface])

  useLayoutEffect(() => {
    candidateList.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' })
  }, [suggestionIndex, suggestions.length, showCandidateSurface])

  return (
    <motion.section className="composer-region" aria-label="任务输入区"
      layout={reducedMotion ? false : 'position'} layoutDependency={centered}
      transition={{ layout: disclosureTransition(true, reducedMotion) }}>
      {state.session?.runtime.pendingQuestion && <QuestionPanel key={state.session.runtime.pendingQuestion.itemId} request={state.session.runtime.pendingQuestion} state={state} />}
      {!state.session?.runtime.pendingQuestion && <>
      <AnimatePresence initial={false}>{queue.length > 0 && <QueuedInputs key={state.selectedSessionId} controls={queue} state={state} />}</AnimatePresence>
      <div ref={candidateAnchor} className="composer-candidate-anchor">
        {centered && <div className="composer-project" hidden={showCandidateSurface}><WorkspacePicker state={state} /></div>}
        <PickerSurface open={showCandidateSurface}>
          <div ref={candidateList} className="composer-candidates" id="composer-suggestions" role="listbox" aria-label="输入建议">
            {suggestions.map((candidate, index) => (
              <button
                type="button"
                role="option"
                className={skillMenu ? 'skill-candidate' : undefined}
                aria-selected={suggestionIndex === index}
                id={`composer-suggestion-${index}`}
                key={candidate.value}
                onMouseDown={event => event.preventDefault()}
                {...selectionGuard(() => insertCandidate(candidate.value))}
              >
                <strong>{skillMenu ? '/' : '@'}{candidate.value}</strong><span>{candidate.description}</span>
              </button>
            ))}
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
        </PickerSurface>
      </div>
      <div {...imageInput.handlers} className={`composer-card${state.selectedWorkspaceId === null ? ' is-unavailable' : ''}`}>
        <DraftImages images={imageDraft} remove={index => { if (sessionId !== null) appStore.setImages(sessionId, imageDraft.filter((_, at) => at !== index)) }} />
        <textarea
          ref={textarea}
          disabled={state.selectedWorkspaceId === null || state.drafts === null}
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
              else if (phase === 'running' && draft.trim() === '' && imageDraft.length === 0 && (event.ctrlKey || event.metaKey)) void appStore.sendNow()
              else {
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
            {imageInput.picker}
            <ComposerTools key={state.selectedSessionId ?? state.selectedWorkspaceId} imageInput={imageInput}
              theme={state.theme} occupancy={occupancy}
              compactDisabled={!appStore.modelAvailable() || state.session === null || !hasTurns || state.connection !== 'ready' || phase !== 'idle' || state.pendingActions.has(pendingKey('session.compact', sessionOrigin))} />

          </div>
          <div className="composer-actions">
            <ModelPicker state={state} />
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
      </>}
      {usage !== null && <ComposerStats usage={usage} />}
    </motion.section>
  )
}

function ContextRing({ percent = 0 }: { percent?: number }) {
  return <svg className="context-ring" viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="7" /><circle cx="10" cy="10" r="7" pathLength="100" strokeDasharray={`${percent} 100`} /></svg>
}

function ComposerTools({ compactDisabled, theme, occupancy, imageInput }: { compactDisabled: boolean; theme: AppState['theme']; occupancy: { used: number; capacity: number; percent: number } | null; imageInput: ReturnType<typeof useImageInput> }) {
  const [expanded, setExpanded] = useState(false)
  const [confirming, setConfirming] = useState(false)
  const [contextOpen, setContextOpen] = useState(false)
  const changeExpanded = useCallback((next: boolean) => {
    setConfirming(false)
    setContextOpen(false)
    setExpanded(next)
  }, [])
  const toolsRoot = useRef<HTMLElement>(null)
  const toggleButton = useRef<HTMLButtonElement>(null)
  const imageButton = useRef<HTMLButtonElement>(null)
  const compactButton = useRef<HTMLButtonElement>(null)
  const reducedMotion = useReducedMotion()
  const guard = useSelectionGuard()

  useEffect(() => { if (compactDisabled) setConfirming(false) }, [compactDisabled])
  useDismissOnOutside(compactButton, confirming, () => setConfirming(false), { captureClick: true })

  useEffect(() => {
    if (!expanded) return
    if (document.activeElement === toggleButton.current) imageButton.current?.focus({ preventScroll: true })
  }, [expanded])
  useDismissOnOutside(toolsRoot, expanded, () => changeExpanded(false))

  return <aside ref={toolsRoot} className="composer-tools" aria-label="任务工具" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) changeExpanded(false)
  }} onKeyDown={event => {
    if (event.key === 'Escape') {
      event.preventDefault()
      event.stopPropagation()
      if (contextOpen) { setContextOpen(false); compactButton.current?.focus({ preventScroll: true }) }
      else { changeExpanded(false); toggleButton.current?.focus({ preventScroll: true }) }
    } else if (expanded && navigateList(event.key, [...event.currentTarget.querySelectorAll<HTMLButtonElement>('.composer-tools-item')])) {
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
        <button ref={imageButton} type="button" className="composer-tools-item" disabled={imageInput.disabled} {...guard(() => {
          imageInput.open(); changeExpanded(false); toggleButton.current?.focus({ preventScroll: true })
        })}>
          <span className="composer-tools-icon" aria-hidden="true"><ImagePlus size={18} strokeWidth={1.6} /></span>
          <span>添加图片</span>
        </button>
        <button ref={compactButton} type="button" className="composer-tools-item" aria-disabled={compactDisabled}
          aria-label={confirming ? '确认压缩上下文' : '压缩上下文'} aria-describedby="composer-context-usage"
          onPointerEnter={() => setContextOpen(true)} onPointerLeave={() => setContextOpen(false)}
          onFocus={() => setContextOpen(true)} onBlur={() => setContextOpen(false)}
          {...guard(() => {
            if (compactDisabled) return
            if (confirming) { changeExpanded(false); toggleButton.current?.focus({ preventScroll: true }); void appStore.compact() }
            else setConfirming(true)
          })}>
          <span className="composer-tools-icon" aria-hidden="true"><ContextRing percent={occupancy?.percent} /></span>
          <span>{confirming ? '确认压缩上下文' : '上下文压缩'}</span>
        </button>
        <button type="button" className="composer-tools-item" aria-label={theme === 'light' ? '切换深色模式' : '切换浅色模式'} onClick={() => {
          const update = () => flushSync(() => appStore.setTheme(theme === 'light' ? 'dark' : 'light'))
          if (!reducedMotion && document.startViewTransition) document.startViewTransition(update)
          else update()
        }}>
          <span className="composer-tools-icon theme-icon" aria-hidden="true">
            <svg key={theme} width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">{theme === 'light' ? <path d="M20.5 14a8.5 8.5 0 0 1-10.5-10.5A8.5 8.5 0 1 0 20.5 14Z" /> : <><circle cx="12" cy="12" r="4" /><path d="M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M5 19l1.5-1.5m11-11L19 5" /></>}</svg>
          </span>
          <span>{theme === 'light' ? '深色模式' : '浅色模式'}</span>
        </button>
        <button type="button" className="composer-tools-item" {...guard(() => {
          changeExpanded(false)
          toggleButton.current?.focus({ preventScroll: true })
          appStore.setSettingsOpen(true)
        })}>
          <span className="composer-tools-icon" aria-hidden="true"><Settings size={18} strokeWidth={1.6} /></span>
          <span>设置</span>
        </button>
      </div>
    </div>
    {expanded && <span className="context-tooltip" data-open={contextOpen} id="composer-context-usage" role="tooltip">{occupancy ? <><span>上下文窗口：</span><span>{occupancy.percent}% 已用</span><strong>已用 {formatTokenCount(occupancy.used)} token，共 {formatTokenCount(occupancy.capacity)}</strong></> : <span>暂无上下文用量</span>}</span>}
  </aside>
}

/**
 * 输入框下方的会话统计条；有计时或消费记录时显示，运行中请求不打断已有读数。
 * 口径由 sessionUsage 单点定义，悬停说明给出耗时和消费明细。
 */
function ComposerStats({ usage }: { usage: SessionModelUsage }) {
  const hit = cacheHitPercent(usage)
  const rate = generationRate(usage)
  const latency = firstTokenLatency(usage)
  const detail = [
    ...(latency === null ? [] : [`首 token 平均延迟 ${(latency / 1000).toFixed(1)} 秒（${usage.ttftRequests} 次请求）`]),
    ...(usage.usagePresent ? [
      `输入 ${usage.inputTokens.toLocaleString()}（缓存 ${usage.cacheUsageComplete ? usage.cachedInputTokens.toLocaleString() : '未知'}）`,
      `输出 ${usage.outputTokens.toLocaleString()}`,
      `请求耗时 ${(usage.generationMs / 1000).toFixed(1)} 秒`,
    ] : []),
    ...(rate === null ? [] : [`TPS 按有计时记录的请求统计，不含首 token 等待（生成 ${(usage.decodeMs / 1000).toFixed(1)} 秒）`]),
  ].join(' · ')
  return <div className="composer-stats" title={detail}>
    {rate !== null && <span className="composer-stat">{rate.toFixed(1)} TPS</span>}
    {latency !== null && <span className="composer-stat">{(latency / 1000).toFixed(1)}s TTFT</span>}
    {usage.usagePresent && <span className="composer-stat">{formatTokenCount(usage.totalTokens)} token</span>}
    {hit !== null && <span className="composer-stat">缓存命中率 {hit.toFixed(1)}%</span>}
  </div>
}
