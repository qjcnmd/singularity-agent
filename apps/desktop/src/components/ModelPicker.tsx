import '../styles/model-picker.css'
import { ExpandChevron } from './ExpandChevron'
import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react'
import { usePopoverFocus, useSelectionGuard, focusableElements, navigateList } from '../interactions'
import { sortReasoningVariants, selectedModel } from '../modelChoices'
import type { ModelSelection, ModelConfigurationInput, RedactedProvider } from '../protocol'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'

interface ModelChoice {
  provider: RedactedProvider
  model: ModelConfigurationInput
}

interface ModelPickerProps {
  /** 只声明本组件读取的字段：父级按同一份清单订阅。 */
  state: Pick<AppState, 'bootstrap' | 'session' | 'selectedWorkspaceId' | 'selectedSessionId' | 'pendingActions'>
}

export function ModelPicker({ state }: ModelPickerProps) {
  const selection = appStore.modelSelection()
  // 切换任务关闭浮层并丢弃待处理的滑块编辑；同一任务切换模型保留浮层。
  const scope = JSON.stringify([state.selectedWorkspaceId, state.selectedSessionId])
  return <ModelPickerControls key={scope} state={state} selection={selection} />
}

function ModelPickerControls({ state, selection }: ModelPickerProps & {
  selection: ModelSelection | null
}) {
  const [open, setOpen] = useState(false)
  const popupId = useId()
  const popup = useRef<HTMLDivElement>(null)
  const [previewIndex, setPreviewIndex] = useState<number | null>(null)
  const committing = useRef(false)
  const queuedEffort = useRef<number | null>(null)
  const dragging = useRef(false)
  const catalog = state.bootstrap?.modelCatalog
  const currentChoice = selectedModel(catalog, selection)
  const variants = sortReasoningVariants(currentChoice?.model.reasoningVariants)
  const resolvedEffort = currentChoice?.effort ?? null
  const resolvedIndex = variants.findIndex((variant) => variant === resolvedEffort)
  const sliderIndex = previewIndex ?? Math.max(0, resolvedIndex)
  const effortSelected = previewIndex !== null || resolvedIndex >= 0
  const displayedEffort = effortSelected ? variants[Math.round(sliderIndex)] ?? '' : ''
  useEffect(() => {
    if (!committing.current && !dragging.current) setPreviewIndex(null)
  }, [selection, open])
  useLayoutEffect(() => () => { queuedEffort.current = null }, [])
  const selectionGuard = useSelectionGuard()
  const sessionId = state.selectedSessionId
  const origin = actionOrigin.session(sessionId)
  const pending = state.pendingActions.has(pendingKey('session.updateSettings', origin))

  usePopoverFocus(open, popup, '.sg-native-slider:not(:disabled), .sg-menu button:not(:disabled)')

  const chooseModel = async (choice: ModelChoice) => {
    const effort = choice.model.reasoningVariants?.some((variant) => variant === resolvedEffort)
      ? resolvedEffort
      : null
    await appStore.updateSettings({ provider: choice.provider.providerId, model: choice.model.modelId, reasoning: effort })
  }

  const chooseEffort = async (position: number) => {
    const index = Math.round(position)
    setPreviewIndex(index)
    queuedEffort.current = index
    if (committing.current || currentChoice === undefined) return
    committing.current = true
    try {
      while (queuedEffort.current !== null) {
        const next = queuedEffort.current
        queuedEffort.current = null
        const variant = variants[next]
        if (variant === undefined) break
        const nextSelection = { provider: currentChoice.provider.providerId, model: currentChoice.model.modelId, reasoning: variant }
        if (!await appStore.updateSettings(nextSelection)) {
          queuedEffort.current = null
          break
        }
      }
    } finally {
      committing.current = false
      if (!dragging.current) setPreviewIndex(null)
    }
  }

  const modelLabel = currentChoice?.model.modelId ?? '选择模型'
  const effortLabel = resolvedEffort
  const providers = catalog?.providers ?? []

  return (
    <div className="sg-root" onKeyDown={event => {
      if (event.target instanceof HTMLInputElement || !open) return
      const menu = event.currentTarget.querySelector<HTMLElement>('.sg-menu')
      if (menu && navigateList(event.key, focusableElements(menu).filter(node => node.tagName === 'BUTTON'))) event.preventDefault()
    }}>
      <button
        popoverTarget={popupId}
        type="button"
        className="sg-trigger"
        disabled={state.selectedWorkspaceId === null}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={effortLabel ? `${modelLabel} · ${effortLabel}` : modelLabel}
        {...selectionGuard()}
      >
        <span className="sg-triggerLabel">{modelLabel}</span>
        {effortLabel !== null && <span className="sg-triggerEffort">{effortLabel}</span>}
        <ExpandChevron expanded={open} size={12} className="sg-chevron" />
      </button>

      <div ref={popup} id={popupId} popover="auto" className="picker-disclosure model-popover" inert={!open} aria-hidden={!open}
        onToggle={event => setOpen(event.newState === 'open')}><div>
        <div className="sg-menu" role="dialog" aria-label="模型与推理等级">
          <div className="sg-menuBody">
            <div className="sg-groups">
              {providers.length === 0 && <p className="candidate-message">尚未配置模型</p>}
              {providers.map((provider) => (
                <section key={provider.providerId} className="sg-group">
                  <div className="sg-groupTitle">{provider.displayName ?? provider.providerId}</div>
                  {provider.models.map((model) => {
                    const isSelected =
                      provider.providerId === currentChoice?.provider.providerId &&
                      model.modelId === currentChoice.model.modelId
                    return (
                      <button
                        key={model.modelId}
                        type="button"
                        aria-pressed={isSelected}
                        disabled={pending}
                        className="sg-option"
                        {...selectionGuard(() => {
                          void chooseModel({ provider, model })
                        })}
                      >
                        <span className="sg-modelName">{model.modelId}</span>
                        <span className="sg-check">{isSelected && <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><path d="M20 6L9 17l-5-5" /></svg>}</span>
                      </button>
                    )
                  })}
                </section>
              ))}
            </div>
          </div>
          {variants.length > 0 && <div className="sg-divider" />}
          {variants.length > 0 && (
            <div className="sg-effortPad">
              <div className="sg-effortHead"><span className="sg-effortTitle">思考程度</span><strong className="sg-effortValue">{displayedEffort}</strong></div>
              <div className="sg-track">
                <div className="sg-range" aria-hidden="true">
                  <div className="sg-rail" />
                  <div className="sg-fill" style={{ width: !effortSelected ? '0' : variants.length === 1 ? 'calc(100% + 12px)' : sliderIndex <= 0 ? '0' : `calc(${sliderIndex / (variants.length - 1) * 100}% + 12px)` }} />
                  {variants.map((variant, index) => <div key={variant} className={`sg-tick${effortSelected && index <= Math.round(sliderIndex) ? ' sg-tick-on' : ''}`} style={{ left: `${variants.length > 1 ? index / (variants.length - 1) * 100 : 100}%` }} />)}
                  <div className="sg-thumb" style={{ left: `${variants.length > 1 ? sliderIndex / (variants.length - 1) * 100 : 100}%`, opacity: effortSelected ? 1 : 0 }} />
                </div>
                <input className="sg-native-slider" type="range" min={0} max={Math.max(0, variants.length - 1)} step="any" value={sliderIndex}
                  aria-label="思考程度" aria-valuetext={displayedEffort || '未选择'} aria-busy={pending}
                  // 档位连续调整由本组件排队；模型切换保存期间等新模型生效后再调整。
                  disabled={(pending && !committing.current) || (variants.length === 1 && resolvedIndex === 0)}
                  onChange={event => setPreviewIndex(Number(event.currentTarget.value))}
                  onPointerDown={event => { dragging.current = true; event.currentTarget.setPointerCapture(event.pointerId) }}
                  onPointerCancel={() => { dragging.current = false; setPreviewIndex(null) }}
                  onPointerUp={event => { dragging.current = false; void chooseEffort(Number(event.currentTarget.value)) }}
                  onKeyDown={event => {
                    const delta = ['ArrowRight', 'ArrowUp', 'PageUp'].includes(event.key) ? 1 : ['ArrowLeft', 'ArrowDown', 'PageDown'].includes(event.key) ? -1 : 0
                    if (!delta && event.key !== 'Home' && event.key !== 'End') return
                    event.preventDefault()
                    const next = event.key === 'Home' ? 0 : event.key === 'End' ? variants.length - 1 : Math.max(0, Math.min(variants.length - 1, Math.round(previewIndex ?? resolvedIndex) + delta))
                    void chooseEffort(next)
                  }}
                  onBlur={() => { if (dragging.current) { dragging.current = false; void chooseEffort(sliderIndex) } }}
                />
              </div>
            </div>
          )}

        </div>
      </div></div>
    </div>
  )
}
