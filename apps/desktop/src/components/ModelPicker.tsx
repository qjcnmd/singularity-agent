import '../styles/model-picker.css'
import { EffortSlider } from './EffortSlider'
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
  state: Pick<AppState, 'bootstrap' | 'session' | 'selectedWorkspaceRoot' | 'selectedSessionId' | 'pendingActions'>
}

export function ModelPicker({ state }: ModelPickerProps) {
  const selection = appStore.modelSelection()
  // 切换任务关闭浮层并丢弃待处理的滑块编辑；同一任务切换模型保留浮层。
  const scope = JSON.stringify([state.selectedWorkspaceRoot, state.selectedSessionId])
  return <ModelPickerControls key={scope} state={state} selection={selection} />
}

function ModelPickerControls({ state, selection }: ModelPickerProps & {
  selection: ModelSelection | null
}) {
  const [open, setOpen] = useState(false)
  const popupId = useId()
  const popup = useRef<HTMLDivElement>(null)
  const trigger = useRef<HTMLButtonElement>(null)
  // 只在打开前定位一次：水平中心对准按钮中心，底边在按钮上方 8px，左右至少离窗口边缘 8px。
  const placePopover = (node: HTMLElement) => {
    const rect = trigger.current?.getBoundingClientRect()
    if (!rect) return
    const width = node.offsetWidth || 220
    node.style.left = `${Math.min(Math.max(8, rect.left + rect.width / 2 - width / 2), window.innerWidth - width - 8)}px`
    node.style.bottom = `${window.innerHeight - rect.top + 8}px`
  }
  const [previewIndex, setPreviewIndex] = useState<number | null>(null)
  const committing = useRef(false)
  const queuedEffort = useRef<number | null>(null)
  const catalog = state.bootstrap?.modelCatalog
  const currentChoice = selectedModel(catalog, selection)
  const variants = sortReasoningVariants(currentChoice?.model.reasoningVariants)
  const resolvedEffort = currentChoice?.effort ?? null
  const resolvedIndex = variants.findIndex((variant) => variant === resolvedEffort)
  const sliderIndex = previewIndex ?? Math.max(0, resolvedIndex)
  useEffect(() => {
    if (!committing.current) setPreviewIndex(null)
  }, [selection, open])
  useLayoutEffect(() => () => { queuedEffort.current = null }, [])
  const selectionGuard = useSelectionGuard()
  const sessionId = state.selectedSessionId
  const origin = actionOrigin.session(sessionId)
  const pending = state.pendingActions.has(pendingKey('session.updateSettings', origin))

  usePopoverFocus(open, popup, '.sg-option[aria-pressed=true], .sg-menu button:not(:disabled)')

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
      setPreviewIndex(null)
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
        ref={trigger}
        popoverTarget={popupId}
        type="button"
        className="sg-trigger"
        disabled={state.selectedWorkspaceRoot === null}
        aria-haspopup="dialog"
        aria-expanded={open}
        {...selectionGuard()}
      >
        <span className="sg-triggerLabel">{modelLabel}</span>
        {effortLabel !== null && <span className="sg-triggerEffort">· {effortLabel}</span>}
        <ExpandChevron expanded={open} size={12} className="sg-chevron" />
      </button>

      <div ref={popup} id={popupId} popover="auto" className="picker-disclosure model-popover" inert={!open} aria-hidden={!open}
        onBeforeToggle={event => { if (event.newState === 'open') placePopover(event.currentTarget) }}
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
          {variants.length > 0 && <fieldset className="effort-field" disabled={(pending && !committing.current) || (variants.length === 1 && resolvedIndex === 0)}>
            <EffortSlider variants={variants} selectedIndex={sliderIndex} onCommit={index => { void chooseEffort(index) }} />
          </fieldset>}

        </div>
      </div></div>
    </div>
  )
}
