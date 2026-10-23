import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { navigateList, useSelectionGuard, useDismissOnOutside } from '../interactions'
import { RpcFailure } from '../rpcClient'
import type { SessionModelUsage } from '../protocol'
import { actionOrigin, appStore, useAppStore, pendingKey, type AppState } from '../appStore'
import { ModelPicker } from './ModelPicker'
import { WorkspacePicker } from './WorkspacePicker'
import { PickerSurface } from './PickerSurface'
import { ActivityOrb } from './ActivityOrb'
import { motion, useReducedMotion } from 'motion/react'
import { flushSync } from 'react-dom'
import { ImagePlus, Settings } from 'lucide-react'
import { formatTokenCount } from '../copy'
import { cacheHitPercent, firstTokenLatency, generationRate, sessionUsage } from '../sessionUsage'
import { inputTrigger } from '../inputTrigger'
import { useFileCandidates, useSkillCandidates } from '../inputCandidates'
import { disclosureTransition } from '../motion'
import { QuestionPanel } from './QuestionPanel'
import { DraftImages, useImageInput } from './Images'
import { selectedSessionPhase } from '../sessionState'

export const Composer = memo(ComposerView)

function ComposerView({ centered }: { centered: boolean }) {
  const reducedMotion = useReducedMotion()
  const state = useAppStore(['drafts', 'questionDrafts', 'actionErrors', 'pendingActions', 'bootstrap', 'bootstrapLoad', 'connection', 'selectedSessionId', 'selectedWorkspaceId', 'session', 'liveSessions', 'sessionLoad', 'theme', 'workspaceAppearance'])
  const { text: draft, images: imageDraft } = appStore.inputDraft()
  const sessionId = state.selectedSessionId
  const draftKey = appStore.draftKey()
  const pendingQuestion = state.session?.runtime.pendingQuestion
  const inputDisabled = draftKey === null || state.drafts === null
  const imageInput = useImageInput(images => { if (draftKey !== null) appStore.setImages(draftKey, images, true) }, inputDisabled || !!pendingQuestion)
  const phase = selectedSessionPhase(state)
  const busy = phase === 'running' || phase === 'compacting' || phase === 'stopping'
  const hasTurns = (state.session?.facts.length ?? 0) > 0
  const [caret, setCaret] = useState(draft.length)
  const trigger = inputTrigger(draft, caret)
  const fileQuery = trigger?.kind === 'file' ? trigger.query : undefined
  const skillQuery = trigger?.kind === 'skill' ? trigger.query : undefined
  const skillMenu = skillQuery !== undefined
  const files = useFileCandidates(fileQuery, state)
  const skills = useSkillCandidates(skillMenu, state)
  const candidates = skillMenu ? skills : files
  const suggestions: { value: string; description: string }[] = skillMenu
    ? (skills.status === 'ready' ? skills.result.skills : []).filter(skill => skill.name.startsWith(skillQuery)).map(skill => ({ value: skill.name, description: skill.description }))
    : (files.status === 'ready' ? files.result : []).map(file => ({ value: file.path, description: '任务文件' }))
  const [suggestionIndex, setSuggestionIndex] = useState(0)
  const [suggestionsOpen, setSuggestionsOpen] = useState(true)
  const textarea = useRef<HTMLTextAreaElement>(null)
  const candidateAnchor = useRef<HTMLDivElement>(null)
  const candidateList = useRef<HTMLDivElement>(null)
  const candidateRetry = useRef<HTMLButtonElement>(null)
  const selectionGuard = useSelectionGuard()
  const sessionOrigin = actionOrigin.session(sessionId)
  const contextUsage = state.session?.runtime.contextUsage
  const occupancy = contextUsage ? { ...contextUsage, percent: Math.min(100, Math.round(contextUsage.used / contextUsage.capacity * 100)) } : null
  const usage = useMemo(() => sessionUsage(state.session), [state.session])
  useEffect(() => {
    setCaret(textarea.current?.selectionStart ?? draft.length)
  }, [draft, state.selectedSessionId, state.selectedWorkspaceId])
  useEffect(() => setSuggestionIndex((index) => Math.min(index, Math.max(0, suggestions.length - 1))), [suggestions.length])
  const { canSubmit, blockedReason } = appStore.submissionState()

  const insertCandidate = (candidate: typeof suggestions[number]) => {
    if (trigger === null) return
    const insertion = `${trigger.kind === 'skill' ? '/' : '@'}${candidate.value} `
    const position = trigger.start + insertion.length
    appStore.setDraft(draft.slice(0, trigger.start) + insertion + draft.slice(trigger.end))
    setCaret(position)
    requestAnimationFrame(() => { textarea.current?.focus(); textarea.current?.setSelectionRange(position, position) })
    setSuggestionsOpen(false)
  }

  const chooseSuggestion = (index: number) => {
    const suggestion = suggestions[index]
    if (suggestion !== undefined) insertCandidate(suggestion)
  }

  const showCandidateSurface = suggestionsOpen
    && trigger !== null && (skillMenu || suggestions.length > 0 || (fileQuery !== undefined && files.status !== 'idle'))

  useLayoutEffect(() => {
    const anchor = candidateAnchor.current
    const list = candidateList.current
    if (!showCandidateSurface || !anchor || !list) return
    // 浮层避开顶部 52px 的标题栏与侧栏开关区，只使用输入框上方的空间；
    // 输入高度和视口变化都重新量。
    const measure = () => {
      const available = anchor.getBoundingClientRect().top - 52
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
      {!pendingQuestion &&
      <div ref={candidateAnchor} className="composer-candidate-anchor">
        {centered && <div className="composer-project" hidden={showCandidateSurface}><WorkspacePicker state={state} /></div>}
        <PickerSurface open={showCandidateSurface}>
          <div ref={candidateList} className="composer-candidates" onKeyDown={event => {
            if (event.key === 'Escape') {
              event.preventDefault()
              setSuggestionsOpen(false)
              textarea.current?.focus()
            }
          }}>
            <div id="composer-suggestions" role="listbox" aria-label="输入建议" aria-busy={candidates.status === 'loading'}>
            {suggestions.map((candidate, index) => (
              <button
                type="button"
                role="option"
                className={skillMenu ? 'skill-candidate' : undefined}
                aria-selected={suggestionIndex === index}
                id={`composer-suggestion-${index}`}
                key={candidate.value}
                onMouseDown={event => event.preventDefault()}
                {...selectionGuard(() => insertCandidate(candidate))}
              >
                <strong>{skillMenu ? '/' : '@'}{candidate.value}</strong><span>{candidate.description}</span>
              </button>
            ))}
            </div>
            {candidates.status === 'loading' && <p className="candidate-message" role="status">{skillMenu ? '正在读取 Skills…' : '正在搜索文件…'}</p>}
            {skillMenu && skills.status === 'ready' && suggestions.length === 0 && <p className="candidate-message">{skills.result.skills.length === 0 ? '没有可用的 Skills' : '没有匹配的 Skills'}</p>}
            {skillMenu && skills.status === 'ready' && skills.result.diagnostics.map(message => <p className="candidate-message candidate-error" role="alert" key={message}>{message}</p>)}
            {fileQuery !== undefined && files.status === 'ready' && files.result.length === 0 && <p className="candidate-message">没有匹配的文件</p>}
            {candidates.status === 'error' && (
              <div className="candidate-message candidate-error" role="alert">
                <strong>{candidates.error.message}</strong>{candidates.error instanceof RpcFailure && candidates.error.recovery && <span>{candidates.error.recovery}</span>}
                <button ref={candidateRetry} type="button" className="secondary-button" onClick={() => {
                  candidates.retry()
                  textarea.current?.focus()
                }}>{skillMenu ? '重试读取 Skills' : '重试搜索文件'}</button>
              </div>
            )}
          </div>
        </PickerSurface>
      </div>}
      <div {...imageInput.handlers} className={`composer-card${inputDisabled ? ' is-unavailable' : ''}`}>
        {pendingQuestion ? <QuestionPanel key={pendingQuestion.itemId} request={pendingQuestion} state={state} /> : <>
        <DraftImages images={imageDraft} remove={index => { if (draftKey !== null) appStore.setImages(draftKey, imageDraft.filter((_, at) => at !== index)) }} />
        <textarea
          ref={textarea}
          disabled={inputDisabled}
          placeholder={state.selectedWorkspaceId === null ? '请先选择项目' : undefined}
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
            if (showCandidateSurface && candidates.status === 'error' && event.key === 'Tab' && !event.shiftKey) {
              event.preventDefault()
              candidateRetry.current?.focus()
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
              else {
                void appStore.submitDraft()
              }
            }
          }}
          rows={1}
          aria-label="任务说明"
          aria-controls={showCandidateSurface ? 'composer-suggestions' : undefined}
          aria-activedescendant={showCandidateSurface && suggestions.length > 0 ? `composer-suggestion-${suggestionIndex}` : undefined}
        />
        </>}
        <div className="composer-toolbar">
          <div className="composer-context">
            {!pendingQuestion && <>
            {imageInput.picker}
            <ComposerTools key={state.selectedSessionId ?? state.selectedWorkspaceId} imageInput={imageInput}
              theme={state.theme} occupancy={occupancy}
              compactDisabled={!appStore.modelAvailable() || !hasTurns || state.connection !== 'ready' || phase !== 'idle' || state.pendingActions.has(pendingKey('session.compact', sessionOrigin))} />
            </>}

          </div>
          <div className="composer-actions">
            {!pendingQuestion && <ModelPicker state={state} />}
            {busy && (
              <button
                type="button"
                className="stop-button"
                {...selectionGuard(() => { void appStore.stopActive() })}
                disabled={phase === 'stopping' || state.pendingActions.has(pendingKey('session.abort', sessionOrigin))}
                aria-label={phase === 'stopping' ? '正在停止当前回合' : '停止当前回合'}
                title={phase === 'stopping' ? '正在停止当前回合，后台命令继续运行' : '停止当前回合；后台命令继续运行，发送下一条消息后恢复自动续跑'}
              >
                <ActivityOrb fast />
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
              <ActivityOrb />
            </button>}
          </div>
        </div>
      </div>
      {usage !== null && <ComposerStats usage={usage} />}
    </motion.section>
  )
}

function ContextRing({ percent = 0 }: { percent?: number }) {
  return <svg className="context-ring" viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="7" /><circle cx="10" cy="10" r="7" pathLength="100" strokeDasharray={`${percent} 100`} /></svg>
}

function ComposerTools({ compactDisabled, theme, occupancy, imageInput }: { compactDisabled: boolean; theme: AppState['theme']; occupancy: { used: number; capacity: number; percent: number } | null; imageInput: ReturnType<typeof useImageInput> }) {
  const [expanded, setExpanded] = useState(false)
  const [contextOpen, setContextOpen] = useState(false)
  const changeExpanded = useCallback((next: boolean) => {
    setContextOpen(false)
    setExpanded(next)
  }, [])
  const toolsRoot = useRef<HTMLElement>(null)
  const toggleButton = useRef<HTMLButtonElement>(null)
  const imageButton = useRef<HTMLButtonElement>(null)
  const compactButton = useRef<HTMLButtonElement>(null)
  const reducedMotion = useReducedMotion()
  const guard = useSelectionGuard()

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
          aria-label="压缩上下文" aria-describedby="composer-context-usage"
          onPointerEnter={() => setContextOpen(true)} onPointerLeave={() => setContextOpen(false)}
          onFocus={() => setContextOpen(true)} onBlur={() => setContextOpen(false)}
          {...guard(() => {
            if (compactDisabled) return
            changeExpanded(false)
            toggleButton.current?.focus({ preventScroll: true })
            void appStore.compact()
          })}>
          <span className="composer-tools-icon" aria-hidden="true"><ContextRing percent={occupancy?.percent} /></span>
          <span>上下文压缩</span>
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
 * 统计口径由 sessionUsage 定义，悬停说明给出耗时和消费明细。
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
