import { useEffect, useMemo, useRef, useState } from 'react'
import { uiText } from '../copy'
import { useSelectionGuard } from '../interactions'
import type { ControlSnapshot } from '../protocol'
import { workbenchStore, type WorkbenchState } from '../store'
import { ModelPicker } from './ModelPicker'

export function Composer({ state }: { state: WorkbenchState }) {
  const draft = workbenchStore.draft()
  const phase = state.session?.runtime.phase ?? 'idle'
  const intent = workbenchStore.currentDeliveryIntent()
  const queue = state.session?.runtime.pendingControls.filter((control) => control.channel === 'follow_up') ?? []
  const commandQuery = draft.startsWith('/') && !draft.includes('\n') ? draft.toLowerCase() : ''
  const availableCommands = useMemo(
    () => (state.bootstrap?.commands ?? []).filter((command) => command.availability === 'always' || command.availability === phase),
    [phase, state.bootstrap?.commands],
  )
  const commandCandidates = useMemo(
    () => commandQuery === '' ? [] : availableCommands.filter((command) => command.name.startsWith(commandQuery)),
    [availableCommands, commandQuery],
  )
  const fileQuery = /@([^\s@]*)$/.exec(draft)?.[1]
  const suggestions = useMemo(() => [
    ...commandCandidates.map((command) => ({ kind: 'command' as const, value: command.name })),
    ...state.fileCandidates.map((candidate) => ({ kind: 'file' as const, value: candidate.path })),
  ], [commandCandidates, state.fileCandidates])
  const [suggestionIndex, setSuggestionIndex] = useState(0)
  const [suggestionsOpen, setSuggestionsOpen] = useState(true)
  const [modelPickerOpen, setModelPickerOpen] = useState(false)
  const textarea = useRef<HTMLTextAreaElement>(null)
  const selectionGuard = useSelectionGuard()
  const sessionOrigin = state.selectedSessionId === null ? undefined : `session:${state.selectedSessionId}`
  useEffect(() => setSuggestionIndex((index) => Math.min(index, Math.max(0, suggestions.length - 1))), [suggestions.length])
  useEffect(() => {
    if (textarea.current === null) return
    textarea.current.style.height = 'auto'
    textarea.current.style.height = `${Math.min(textarea.current.scrollHeight, 220)}px`
  }, [draft])

  const activeAction = phase === 'running'
    ? intent === 'steer' ? 'session.steer' : 'session.followUp'
    : 'session.submit'
  const submitPending = workbenchStore.isPending(activeAction, sessionOrigin)
  const canSubmit = state.selectedSessionId !== null
    && draft.trim() !== ''
    && phase !== 'compacting'
    && phase !== 'stopping'
    && !submitPending

  const insertCandidate = (text: string) => {
    if (fileQuery !== undefined) {
      workbenchStore.setDraft(draft.slice(0, draft.length - fileQuery.length - 1) + `@${text} `)
    } else {
      workbenchStore.setDraft(`${text} `)
    }
    setSuggestionsOpen(false)
    workbenchStore.clearFileCandidates()
  }

  const invokeCommand = (name: string) => {
    if (name === '/compact') void workbenchStore.compact()
    if (name === '/model') setModelPickerOpen(true)
    if (name === '/help') workbenchStore.setHelpOpen(true)
    workbenchStore.setDraft('')
    setSuggestionsOpen(false)
  }

  const chooseSuggestion = (index: number) => {
    const suggestion = suggestions[index]
    if (suggestion === undefined) return
    if (suggestion.kind === 'command') invokeCommand(suggestion.value)
    else insertCandidate(suggestion.value)
  }

  const showCandidateSurface = suggestionsOpen
    && (suggestions.length > 0 || (fileQuery !== undefined && state.fileCandidateStatus !== 'idle'))

  const insertFileReference = () => {
    const spacer = draft === '' || /\s$/.test(draft) ? '' : ' '
    workbenchStore.setDraft(`${draft}${spacer}@`)
    setSuggestionsOpen(true)
    setSuggestionIndex(0)
    void workbenchStore.searchFiles('')
    requestAnimationFrame(() => textarea.current?.focus())
  }

  return (
    <section className="composer-region" aria-label="任务输入区">
      {queue.length > 0 && <FollowUpQueue controls={queue} state={state} />}
      {showCandidateSurface && (
        <div className="composer-candidates" id="composer-suggestions" role="listbox" aria-label="输入建议">
          {commandCandidates.map((command, index) => (
            <button
              type="button"
              role="option"
              aria-selected={suggestionIndex === index}
              id={`composer-suggestion-${index}`}
              key={command.name}
              {...selectionGuard(() => invokeCommand(command.name))}
            >
              <strong>{command.name}</strong><span>{command.description}</span>
            </button>
          ))}
          {state.fileCandidates.map((candidate, candidateIndex) => {
            const index = commandCandidates.length + candidateIndex
            return (
              <button
                type="button"
                role="option"
                aria-selected={suggestionIndex === index}
                id={`composer-suggestion-${index}`}
                key={candidate.path}
                {...selectionGuard(() => insertCandidate(candidate.path))}
              >
                <strong>@{candidate.path}</strong><span>项目文件</span>
              </button>
            )
          })}
          {fileQuery !== undefined && state.fileCandidateStatus === 'loading' && <p className="candidate-message">正在查找项目文件…</p>}
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
          value={draft}
          onChange={(event) => {
            const value = event.target.value
            workbenchStore.setDraft(value)
            setSuggestionsOpen(true)
            setSuggestionIndex(0)
            const query = /@([^\s@]*)$/.exec(value)?.[1]
            void workbenchStore.searchFiles(query ?? '')
          }}
          onKeyDown={(event) => {
            if (suggestionsOpen && suggestions.length > 0 && (event.key === 'ArrowDown' || event.key === 'ArrowUp')) {
              event.preventDefault()
              const direction = event.key === 'ArrowDown' ? 1 : -1
              setSuggestionIndex((index) => (index + direction + suggestions.length) % suggestions.length)
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
              if (suggestionsOpen && suggestions.length > 0) chooseSuggestion(suggestionIndex)
              else if (canSubmit) void workbenchStore.submitDraft()
            }
          }}
          placeholder={state.selectedSessionId === null
            ? '请先创建一个任务'
            : phase === 'running'
              ? '即时调整当前方向，或把消息排到下一回合…'
              : phase === 'compacting'
                ? '压缩结束后即可发送；这里的草稿会保留…'
                : '描述任务；输入 / 调用命令，@ 引用文件…'}
          disabled={state.selectedSessionId === null}
          rows={1}
          aria-label="任务说明"
          aria-controls={showCandidateSurface ? 'composer-suggestions' : undefined}
          aria-activedescendant={suggestionsOpen && suggestions.length > 0 ? `composer-suggestion-${suggestionIndex}` : undefined}
        />
        <div className="composer-toolbar">
          <div className="composer-context">
            <button
              type="button"
              className="composer-plus"
              aria-label="插入文件引用"
              title="引用项目文件"
              disabled={state.selectedSessionId === null}
              {...selectionGuard(insertFileReference)}
            >
              +
            </button>
            <span className="access-chip" title={uiText.fullLocalAccessDetail}><span aria-hidden="true">♢</span>{uiText.fullLocalAccess}</span>
            {phase === 'running' && (
              <div className="intent-switch" role="group" aria-label="投递方式">
                <button type="button" aria-pressed={intent === 'steer'} {...selectionGuard(() => workbenchStore.setDeliveryIntent('steer'))}>即时转向</button>
                <button type="button" aria-pressed={intent === 'follow_up'} {...selectionGuard(() => workbenchStore.setDeliveryIntent('follow_up'))}>后续消息</button>
              </div>
            )}
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
                <span className="stop-icon" aria-hidden="true" />
              </button>
            )}
            <button
              type="button"
              className="submit-button"
              disabled={!canSubmit}
              aria-label={phase === 'running' ? intent === 'steer' ? '即时转向' : '加入后续消息' : '发送消息'}
              title={phase === 'running' ? intent === 'steer' ? '即时转向' : '加入后续消息' : '发送'}
              {...selectionGuard(() => { void workbenchStore.submitDraft() })}
            >
              <span className="submit-arrow" aria-hidden="true">↑</span>
            </button>
          </div>
        </div>
        {phase === 'compacting' && <p className="composer-state-note">你仍可编辑草稿。上下文压缩完成后，点击“发送”开始下一回合。</p>}
      </div>
    </section>
  )
}

function FollowUpQueue({ controls, state }: { controls: ControlSnapshot[]; state: WorkbenchState }) {
  return (
    <div className="follow-up-queue" aria-label="后续消息队列">
      <header><strong>后续消息</strong><span>{controls.length} 条等待处理</span></header>
      {controls.map((control, index) => <QueueRow key={control.controlId} control={control} index={index} state={state} />)}
    </div>
  )
}

function QueueRow({ control, index, state }: { control: ControlSnapshot; index: number; state: WorkbenchState }) {
  const [editing, setEditing] = useState(false)
  const [text, setText] = useState(control.text ?? '')
  const selectionGuard = useSelectionGuard()
  const sessionId = state.selectedSessionId ?? ''
  const origin = `control:${sessionId}:${control.controlId}`
  const pending = ['session.queueReplace', 'session.queueSendNow', 'session.queueWithdraw']
    .some((method) => workbenchStore.isPending(method, origin))
  const error = state.actionErrors[origin]

  const save = async () => {
    if (text.trim() === '') return
    const accepted = await workbenchStore.replace(control.controlId, text)
    if (accepted) setEditing(false)
  }

  return (
    <div className="queue-row">
      <span className="queue-index">{index + 1}</span>
      {editing ? (
        <input value={text} onChange={(event) => setText(event.target.value)} aria-label={`编辑第 ${index + 1} 条后续消息`} />
      ) : <span className="queue-text">{control.text}</span>}
      <span className="queue-actions">
        {editing ? (
          <button type="button" className="quiet-button" disabled={pending || text.trim() === ''} {...selectionGuard(() => { void save() })}>保存</button>
        ) : (
          <button type="button" className="quiet-button" disabled={pending} {...selectionGuard(() => setEditing(true))}>编辑</button>
        )}
        <button type="button" className="quiet-button" disabled={pending} {...selectionGuard(() => { void workbenchStore.sendNow(control.controlId) })}>立即发送</button>
        <button type="button" className="quiet-button danger" disabled={pending} {...selectionGuard(() => { void workbenchStore.withdraw(control.controlId) })}>撤回</button>
      </span>
      {error !== undefined && <div className="queue-error" role="alert"><strong>{error.message}</strong><span>{error.recovery}</span></div>}
    </div>
  )
}
