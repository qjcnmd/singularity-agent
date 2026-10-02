import { useLayoutEffect, useRef, useState, type FormEvent } from 'react'
import { ChevronLeft, ChevronRight, Pencil } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { PendingQuestion, UserQuestionAnswer } from '../protocol'

/** 一次显示一道题；答案在同一次提交中交给当前等待的工具调用。 */
export function QuestionPanel({ request, state }: {
  request: PendingQuestion
  state: Pick<AppState, 'selectedSessionId' | 'pendingActions' | 'actionErrors' | 'connection'>
}) {
  const [index, setIndex] = useState(0)
  const [answers, setAnswers] = useState<UserQuestionAnswer[]>(() => request.questions.map(question => ({ id: question.id, selected: [], text: '', skipped: false })))
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

  useLayoutEffect(() => {
    body.current?.scrollTo(0, 0)
    body.current?.querySelector<HTMLElement>('button, textarea')?.focus({ preventScroll: true })
  }, [index])

  function update(patch: Partial<UserQuestionAnswer>) {
    const next = answers.map((value, at) => at === index ? { ...value, ...patch } : value)
    setAnswers(next)
    return next
  }

  function advance(values = answers) {
    if (!last) { setIndex(index + 1); return }
    const missing = values.findIndex(value => !value.skipped && value.selected.length === 0 && !value.text.trim())
    if (missing >= 0) { setIndex(missing); return }
    void appStore.answerQuestion(request.itemId, values)
  }

  function choose(label: string) {
    update({ selected: question.multiSelect
      ? answer.selected.includes(label) ? answer.selected.filter(value => value !== label) : [...answer.selected, label]
      : [label], text: question.multiSelect ? answer.text : '', skipped: false })
    if (!question.multiSelect && !last) setIndex(index + 1)
  }

  function submit(event: FormEvent) {
    event.preventDefault()
    if (answered && !disabled) advance()
  }

  return <form className="question-panel" aria-label="回答问题" onSubmit={submit} onKeyDown={event => {
    if (event.key === 'Escape' && index > 0) { event.preventDefault(); event.stopPropagation(); setIndex(index - 1) }
  }}>
    <header><strong id="current-question">{question.question}</strong><button type="button" className="quiet-button" disabled={stopping} onClick={() => void appStore.stopActive()}>{stopping ? '正在停止…' : '停止任务'}</button></header>
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
        {question.options.length > 0 && <span className={`question-option-mark${question.multiSelect ? ' question-checkbox' : ''}`} aria-hidden="true">{question.multiSelect ? answer.text.trim() ? '✓' : '' : <Pencil size={13} />}</span>}
        <textarea key={question.id} aria-label={`${question.question}：填写回答`} value={answer.text}
        placeholder={question.options.length ? '填写其他答案…' : '填写你的回答…'} rows={1}
        onChange={event => update({ text: event.target.value, selected: question.multiSelect ? answer.selected : [], skipped: false })}
        onKeyDown={event => {
          if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing && event.nativeEvent.keyCode !== 229) {
            event.preventDefault()
            if (answered && !disabled) advance()
          }
        }} />
      </label>
    </fieldset>
    <footer>
      <nav className="question-pager" aria-label="问题分页">
        <button type="button" className="icon-button" aria-label="上一题" disabled={index === 0 || disabled} onClick={() => setIndex(index - 1)}><ChevronLeft size={16} /></button>
        <span aria-live="polite">{index + 1} / {request.questions.length}</span>
        <button type="button" className="icon-button" aria-label="下一题" disabled={last || disabled} onClick={() => setIndex(index + 1)}><ChevronRight size={16} /></button>
      </nav>
      {error && <p role="alert" className="form-error">{error.message}</p>}
      <div className="question-actions">
        <button type="button" className="quiet-button" disabled={disabled} onClick={() => advance(update({ selected: [], text: '', skipped: true }))}>跳过</button>
        <button type="submit" className="primary-button" disabled={!answered || disabled}>{submitting ? '提交中…' : last ? '提交回答' : '继续'}</button>
      </div>
    </footer>
  </form>
}
