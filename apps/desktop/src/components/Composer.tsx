import { memo, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { actionOrigin, appStore, useAppStore, pendingKey } from '../appStore'
import { ModelPicker } from './ModelPicker'
import { WorkspacePicker } from './WorkspacePicker'
import { PickerSurface } from './PickerSurface'
import { WaterOrb } from './orb/WaterOrb'
import { motion, useReducedMotion } from 'motion/react'
import { inputTrigger } from '../inputTrigger'
import { useSkillCandidates } from '../inputCandidates'
import { smooth } from '../motion'
import { DraftImages, FileChips, useAttachmentInput } from './Images'
import { selectedSessionPhase } from '../sessionState'

export const Composer = memo(ComposerView)

function ComposerView({ accent }: { accent: string }) {
  const reducedMotion = useReducedMotion()
  const state = useAppStore(['drafts', 'pendingActions', 'bootstrap', 'bootstrapLoad', 'connection', 'selectedSessionId', 'selectedWorkspaceRoot', 'session', 'liveSessions', 'sessionLoad'])
  const { text: draft, images: imageDraft, files: fileDraft } = appStore.inputDraft()
  const sessionId = state.selectedSessionId
  const draftKey = appStore.draftKey()
  const pendingQuestion = state.session?.runtime.pendingQuestion
  const inputDisabled = draftKey === null || state.drafts === null
  const attachmentInput = useAttachmentInput(images => { if (draftKey !== null) appStore.setImages(draftKey, images, true) },
    paths => { if (draftKey !== null) appStore.setFiles(draftKey, paths, true) }, inputDisabled || !!pendingQuestion)
  const phase = selectedSessionPhase(state)
  const busy = phase === 'running' || phase === 'compacting' || phase === 'stopping'
  const [caret, setCaret] = useState(draft.length)
  const trigger = pendingQuestion ? null : inputTrigger(draft, caret)
  const skills = useSkillCandidates(trigger !== null, state)
  const suggestions = trigger !== null && skills.status === 'ready'
    ? skills.result.skills.filter(skill => skill.name.startsWith(trigger.query))
    : []
  const [suggestionIndex, setSuggestionIndex] = useState(0)
  const [suggestionsOpen, setSuggestionsOpen] = useState(true)
  const textarea = useRef<HTMLTextAreaElement>(null)
  const candidateAnchor = useRef<HTMLDivElement>(null)
  const candidateList = useRef<HTMLDivElement>(null)
  const candidateRetry = useRef<HTMLButtonElement>(null)
  const sessionOrigin = actionOrigin.session(sessionId)
  const contextUsage = state.session?.runtime.contextUsage
  const occupancy = contextUsage ? { ...contextUsage, percent: Math.min(100, Math.round(contextUsage.used / contextUsage.capacity * 100)) } : null
  useEffect(() => {
    setCaret(textarea.current?.selectionStart ?? draft.length)
  }, [draft, state.selectedSessionId, state.selectedWorkspaceRoot])
  useEffect(() => setSuggestionIndex((index) => Math.min(index, Math.max(0, suggestions.length - 1))), [suggestions.length])
  const { canSubmit, blockedReason } = appStore.submissionState()

  const insertCandidate = (candidate: typeof suggestions[number]) => {
    if (trigger === null) return
    const insertion = `/${candidate.name} `
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

  const showCandidateSurface = suggestionsOpen && trigger !== null

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

  const region = useRef<HTMLElement>(null)
  useEffect(() => {
    const node = region.current!
    const main = node.closest<HTMLElement>('.app-main')!
    const observer = new ResizeObserver(() => main.style.setProperty('--composer-height', `${node.offsetHeight}px`))
    observer.observe(node)
    return () => observer.disconnect()
  }, [])
  const showsStop = busy && (pendingQuestion || (draft.trim() === '' && imageDraft.length === 0 && fileDraft.length === 0))
  const orb = <WaterOrb size={40} color={accent} percent={occupancy?.percent ?? 0} compacting={phase === 'compacting'}
    enabled={showsStop ? phase !== 'stopping' && !state.pendingActions.has(pendingKey('session.abort', sessionOrigin)) : canSubmit}
    label={showsStop ? phase === 'stopping' ? '正在停止当前回合' : '停止当前回合' : blockedReason ?? '发送消息'}
    onPress={() => { if (showsStop) void appStore.stopActive(); else void appStore.submitDraft() }} />
  return (
    <motion.section ref={region} className="composer-region" aria-label="任务输入区"
      layout={reducedMotion ? false : 'position'}
      transition={{ layout: reducedMotion ? { duration: 0 } : smooth }}>
      <div ref={candidateAnchor} className="composer-candidate-anchor">
        <PickerSurface open={showCandidateSurface}>
          <div ref={candidateList} className="composer-candidates" onKeyDown={event => {
            if (event.key === 'Escape') {
              event.preventDefault()
              setSuggestionsOpen(false)
              textarea.current?.focus()
            }
          }}>
            <div id="composer-suggestions" role="listbox" aria-label="输入建议" aria-busy={skills.status === 'loading'}>
            {suggestions.map((candidate, index) => (
              <button
                type="button"
                role="option"
                aria-selected={suggestionIndex === index}
                id={`composer-suggestion-${index}`}
                key={candidate.name}
                onPointerMove={() => setSuggestionIndex(index)}
                onMouseDown={event => event.preventDefault()}
                onClick={() => insertCandidate(candidate)}
              >
                <strong>/{candidate.name}</strong><span>{candidate.description}</span>
              </button>
            ))}
            </div>
            {skills.status === 'loading' && <p className="candidate-message" role="status">正在读取 Skills…</p>}
            {skills.status === 'ready' && suggestions.length === 0 && <p className="candidate-message">{skills.result.skills.length === 0 ? '没有可用的 Skills' : '没有匹配的 Skills'}</p>}
            {skills.status === 'ready' && skills.result.diagnostics.map(message => <p className="candidate-message candidate-error" role="alert" key={message}>{message}</p>)}
            {skills.status === 'error' && (
              <div className="candidate-message candidate-error" role="alert">
                <strong>{skills.error.message}</strong>
                <button ref={candidateRetry} type="button" className="secondary-button" onClick={() => {
                  skills.retry()
                  textarea.current?.focus()
                }}>重试读取 Skills</button>
              </div>
            )}
          </div>
        </PickerSurface>
      </div>
      <div {...attachmentInput.handlers} className={`composer-card${inputDisabled ? ' is-unavailable' : ''}`}>
        <DraftImages images={imageDraft} remove={index => { if (draftKey !== null) appStore.setImages(draftKey, imageDraft.filter((_, at) => at !== index)) }} />
        <FileChips files={fileDraft} remove={index => { if (draftKey !== null) appStore.setFiles(draftKey, fileDraft.filter((_, at) => at !== index)) }} />
        <textarea
          ref={textarea}
          disabled={inputDisabled}
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
            if (showCandidateSurface && skills.status === 'error' && event.key === 'Tab' && !event.shiftKey) {
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
        <div className="composer-toolbar">
          <div className="composer-context">
            {attachmentInput.picker}
            <button type="button" className="attach-button" aria-label="添加附件" title="添加附件" disabled={attachmentInput.disabled} onClick={attachmentInput.open}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" aria-hidden="true"><path d="M5 12h14M12 5v14" /></svg>
            </button>
            {sessionId === null && <WorkspacePicker state={state} />}
          </div>
          <div className="composer-actions"><ModelPicker state={state} />{orb}</div>
        </div>
      </div>
    </motion.section>
  )
}
