import { useLayoutEffect, useRef, type FormEvent } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { smooth } from '../motion'
import { ChevronLeft, ChevronRight, Pencil } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, useAppStore, type AppState } from '../appStore'
import type { PendingQuestion, UserQuestionAnswer } from '../protocol'

/** Agent 的提问：在时间线里显示成助手气泡，逐题分页填写，完成后统一提交给等待中的工具调用。 */
export function QuestionPanel({ request }: { request: PendingQuestion }) {
  const state = useAppStore(['selectedSessionId', 'questionDrafts', 'pendingActions', 'actionErrors', 'connection'])
  const reducedMotion = useReducedMotion()
  const sessionId = state.selectedSessionId!
  const saved = state.questionDrafts[sessionId]
  const draft: AppState['questionDrafts'][string] = saved?.itemId === request.itemId ? saved : {
    itemId: request.itemId, index: 0,
    answers: request.questions.map(question => ({ id: question.id, selected: [], text: '', skipped: false })),
  }
  const { index, answers } = draft
  const setIndex = (index: number, values = answers) => appStore.setQuestionDraft(sessionId, { ...draft, answers: values, index })
  const body = useRef<HTMLFieldSetElement>(null)
  const origin = actionOrigin.control(state.selectedSessionId, request.itemId)
  const submitting = state.pendingActions.has(pendingKey('session.answerQuestion', origin))
  const stopping = state.pendingActions.has(pendingKey('session.abort', actionOrigin.session(state.selectedSessionId)))
  const disabled = submitting || stopping || state.connection !== 'ready'
  const question = request.questions[index]
  const answer = answers[index]
  const answered = answer.selected.length > 0 || answer.text.trim().length > 0
  const last = index === request.questions.length - 1
  const error = state.actionErrors[origin]

  // 气泡出现在时间线里时不抢输入框的焦点；翻到下一题后焦点跟到新题的第一个选项。
  const shownIndex = useRef(index)
  useLayoutEffect(() => {
    if (shownIndex.current === index) return
    shownIndex.current = index
    body.current?.scrollTo(0, 0)
    body.current?.querySelector<HTMLElement>('button, textarea')?.focus({ preventScroll: true })
  }, [index])

  function update(patch: Partial<UserQuestionAnswer>, nextIndex = index) {
    const next = answers.map((value, at) => at === index ? { ...value, ...patch } : value)
    setIndex(nextIndex, next)
  }

  function advance(skip = false) {
    const values = skip ? answers.map((value, at) => at === index
      ? { ...value, selected: [], text: '', skipped: true } : value) : answers
    if (!last) { setIndex(index + 1, values); return }
    const missing = values.findIndex(value => !value.skipped && value.selected.length === 0 && !value.text.trim())
    if (missing >= 0) { setIndex(missing, values); return }
    // 提交失败时仍保留本题的跳过状态，方便继续填写或重试。
    if (skip) setIndex(index, values)
    void appStore.answerQuestion(request.itemId, values)
  }

  function choose(label: string) {
    update({ selected: question.multiSelect
      ? answer.selected.includes(label) ? answer.selected.filter(value => value !== label) : [...answer.selected, label]
      : [label], text: question.multiSelect ? answer.text : '', skipped: false }, !question.multiSelect && !last ? index + 1 : index)
  }

  function submit(event: FormEvent) {
    event.preventDefault()
    if (answered && !disabled) advance()
  }

  return <form className="message-bubble question-panel" aria-label="回答问题" onSubmit={submit} onKeyDown={event => {
    if (event.key === 'Escape' && index > 0) { event.preventDefault(); event.stopPropagation(); setIndex(index - 1) }
  }}>
    <AnimatePresence mode="popLayout" initial={false}>
      <motion.div key={question.id} className="question-body"
        initial={{ opacity: 0, x: reducedMotion ? 0 : 24 }} animate={{ opacity: 1, x: 0 }}
        exit={{ opacity: 0, x: reducedMotion ? 0 : -24 }} transition={reducedMotion ? { duration: 0 } : smooth}>
        <strong id="current-question" className="question-title">{question.question}</strong>
        {question.multiSelect && <span className="question-hint">可多选</span>}
        <fieldset ref={body} className="question-panel-body" aria-labelledby="current-question" disabled={disabled}>
          {question.options.length > 0 && <div className="question-options" role={question.multiSelect ? 'group' : 'radiogroup'} aria-label="答案选项">
            {question.options.map((option, at) => <button key={option.label} type="button" className="question-answer-row"
              role={question.multiSelect ? 'checkbox' : 'radio'} aria-checked={answer.selected.includes(option.label)}
              onClick={() => choose(option.label)} onKeyDown={event => {
                if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return
                event.preventDefault()
                const buttons = [...event.currentTarget.parentElement!.querySelectorAll<HTMLButtonElement>('button')]
                buttons[(at + (event.key === 'ArrowDown' ? 1 : buttons.length - 1)) % buttons.length].focus()
              }}>
              <span className="question-option-mark" aria-hidden="true">{question.multiSelect ? answer.selected.includes(option.label) ? '✓' : '' : at + 1}</span>
              <span>{option.label}{option.description && <small>{option.description}</small>}</span>
            </button>)}
          </div>}
          <label className="question-answer-row question-custom-answer" data-selected={answer.text.trim().length > 0}>
            <span className="question-option-mark" aria-hidden="true"><Pencil size={12} /></span>
            <textarea key={question.id} aria-label={`${question.question}：填写回答`} value={answer.text}
            rows={1}
            onChange={event => update({ text: event.target.value, selected: question.multiSelect ? answer.selected : [], skipped: false })}
            onKeyDown={event => {
              if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing && event.nativeEvent.keyCode !== 229) {
                event.preventDefault()
                if (answered && !disabled) advance()
              }
            }} />
          </label>
        </fieldset>
      </motion.div>
    </AnimatePresence>
    <footer>
      {request.questions.length > 1 && <nav className="question-pager" aria-label="问题分页">
        <button type="button" className="icon-button small" aria-label="上一题" disabled={index === 0 || disabled} onClick={() => setIndex(index - 1)}><ChevronLeft size={15} /></button>
        <span aria-live="polite">{index + 1} / {request.questions.length}</span>
        <button type="button" className="icon-button small" aria-label="下一题" disabled={last || disabled} onClick={() => setIndex(index + 1)}><ChevronRight size={15} /></button>
      </nav>}
      {error && <p role="alert" className="form-error">{error.message}</p>}
      <div className="question-actions">
        <button type="button" className="quiet-button" disabled={disabled} onClick={() => advance(true)}>跳过</button>
        <button type="submit" className="primary-button small" disabled={!answered || disabled}>{submitting ? '提交中…' : last ? '提交回答' : '继续'}</button>
      </div>
    </footer>
  </form>
}
