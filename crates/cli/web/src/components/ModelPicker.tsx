import '../styles/model-picker.css'
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useSelectionGuard, useTransientFocus, focusableElements, navigateList } from '../interactions'
import { reasoningChoices } from '../modelChoices'
import type { RedactedModel, RedactedProvider } from '../protocol'
import { workbenchStore, type WorkbenchState } from '../store'

interface SelectorParts {
  providerId: string
  modelId: string
  effort: string | null
}

interface ModelChoice {
  provider: RedactedProvider
  model: RedactedModel
}

interface ModelPickerProps {
  state: WorkbenchState
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function ModelPicker(props: ModelPickerProps) {
  const { state } = props
  const selector = parseSelector(state.session?.runtime.selector ?? state.bootstrap?.modelCatalog.defaultSelector ?? null)
  // A task/model owns its pending slider edits; only effort changes reuse that queue.
  const scope = JSON.stringify([state.selectedWorkspaceId, state.selectedSessionId, selector?.providerId, selector?.modelId])
  return <ModelPickerControls key={scope} {...props} />
}

function ModelPickerControls({ state, open, onOpenChange }: ModelPickerProps) {
  const root = useRef<HTMLDivElement>(null)
  const [previewIndex, setPreviewIndex] = useState<number | null>(null)
  const committing = useRef(false)
  const queuedEffort = useRef<number | null>(null)
  const dragging = useRef(false)
  const catalog = state.bootstrap?.modelCatalog
  const selector = state.session?.runtime.selector ?? catalog?.defaultSelector ?? null
  const parsed = parseSelector(selector)
  const choices = useMemo(
    () => catalog?.providers.flatMap((provider) => provider.models.map((model) => ({ provider, model }))) ?? [],
    [catalog?.providers],
  )
  const currentChoice = choices.find(
    ({ provider, model }) => provider.providerId === parsed?.providerId && model.modelId === parsed.modelId,
  )
  const variants = reasoningChoices(currentChoice?.model)
  const resolvedEffort = parsed?.effort ?? currentChoice?.model.defaultVariant ?? variants[0]?.id ?? null
  const resolvedIndex = Math.max(0, variants.findIndex((variant) => variant.id === resolvedEffort))
  const sliderIndex = previewIndex ?? resolvedIndex
  useEffect(() => {
    if (!committing.current && !dragging.current) setPreviewIndex(null)
  }, [selector, open])
  useLayoutEffect(() => () => { queuedEffort.current = null }, [])
  const selectionGuard = useSelectionGuard()
  const sessionId = state.selectedSessionId
  const origin = sessionId === null ? undefined : `session:${sessionId}`
  const pending = workbenchStore.isPending('session.updateSettings', origin)
  const error = origin === undefined ? undefined : state.actionErrors[origin]

  useTransientFocus(open, () => onOpenChange(false), root, node => node.querySelector<HTMLElement>('.rsm-native-slider:not(:disabled), .rsm-menu button:not(:disabled)'))
  useEffect(() => {
    if (!open) return
    const close = (event: PointerEvent) => {
      if (event.target instanceof Node && !root.current?.contains(event.target)) onOpenChange(false)
    }
    document.addEventListener('pointerdown', close)
    return () => document.removeEventListener('pointerdown', close)
  }, [onOpenChange, open])

  const chooseModel = async (choice: ModelChoice) => {
    const enabled = reasoningChoices(choice.model)
    const effort = enabled.some((variant) => variant.id === resolvedEffort)
      ? resolvedEffort
      : choice.model.defaultVariant ?? enabled[0]?.id ?? null
    await workbenchStore.updateSettings(composeSelector(choice.provider.providerId, choice.model.modelId, effort))
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
        const nextSelector = composeSelector(currentChoice.provider.providerId, currentChoice.model.modelId, variant.id)
        if (workbenchStore.getSnapshot().session?.runtime.selector === nextSelector) continue
        if (!await workbenchStore.updateSettings(nextSelector)) {
          queuedEffort.current = null
          break
        }
      }
    } finally {
      committing.current = false
      if (!dragging.current) setPreviewIndex(null)
    }
  }

  const modelLabel = currentChoice?.model.displayName ?? currentChoice?.model.modelId ?? parsed?.modelId ?? '选择模型'
  const effortLabel = resolvedEffort === null ? null : formatEffort(resolvedEffort)
  const providers = catalog?.providers ?? []

  return (
    <div className="rsm-root" ref={root} onKeyDown={event => {
      if (event.target instanceof HTMLInputElement || !open) return
      const menu = event.currentTarget.querySelector<HTMLElement>('.rsm-menu')
      if (menu && navigateList(event.key, focusableElements(menu).filter(node => node.tagName === 'BUTTON'))) event.preventDefault()
    }}>
      <button
        type="button"
        className={`rsm-trigger ${open ? 'is-active' : ''}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={effortLabel ? `${modelLabel} · ${effortLabel}` : modelLabel}
        {...selectionGuard(() => {
          if (open) {
            onOpenChange(false)
          } else {
            onOpenChange(true)
          }
        })}
      >
        <span className="rsm-triggerLabel">{modelLabel}</span>
        {effortLabel !== null && <span className="rsm-triggerEffort">{effortLabel}</span>}
        <svg
          className={`rsm-chevron ${open ? 'rsm-chevronOpen' : ''}`}
          width="12"
          height="12"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.2"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <polyline points="6 9 12 15 18 9" />
        </svg>
      </button>

        <div className={`rsm-menu ${open ? 'is-open' : ''}`} role="dialog" aria-label="模型与推理等级" aria-hidden={!open} inert={!open}>
            <div className="rsm-menuBody">
              <div className="rsm-groups">
                {providers.length === 0 && <p className="candidate-message">尚未配置模型</p>}
                {providers.map((provider) => (
                  <section key={provider.providerId} className="rsm-group">
                    <div className="rsm-groupTitle">{provider.displayName ?? provider.providerId}</div>
                    {provider.models.map((model) => {
                      const isSelected =
                        provider.providerId === currentChoice?.provider.providerId &&
                        model.modelId === currentChoice.model.modelId
                      return (
                        <button
                          key={model.modelId}
                          type="button"
                          aria-pressed={isSelected}
                          disabled={!provider.credentialConfigured || pending}
                          className={`rsm-option ${isSelected ? 'is-selected' : ''}`}
                          {...selectionGuard(() => {
                            void chooseModel({ provider, model })
                          })}
                        >
                          <span className="rsm-modelName">{model.displayName ?? model.modelId}</span>
                          <span className="rsm-check">{isSelected && <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><path d="M20 6L9 17l-5-5" /></svg>}</span>
                        </button>
                      )
                    })}
                  </section>
                ))}
              </div>
            </div>
          {variants.length > 0 && <div className="rsm-divider" />}
          {variants.length > 0 && (
            <div className="rsm-effortPad">
              <div className="rsm-effortHead"><span className="rsm-effortTitle">推理等级</span><strong className="rsm-effortValue">{formatEffort(variants[Math.round(sliderIndex)]?.id ?? resolvedEffort ?? '默认')}</strong></div>
              <div className="rsm-track">
                <div className="rsm-range" aria-hidden="true">
                  <div className="rsm-rail" />
                  <div className="rsm-fill" style={{ width: variants.length === 1 ? 'calc(100% + 12px)' : sliderIndex <= 0 ? '0' : `calc(${sliderIndex / (variants.length - 1) * 100}% + 12px)` }} />
                  {variants.map((variant, index) => <div key={variant.id} className={`rsm-tick${index <= Math.round(sliderIndex) ? ' rsm-tick-on' : ''}`} style={{ left: `${variants.length > 1 ? index / (variants.length - 1) * 100 : 100}%` }} />)}
                  <div className="rsm-thumb" style={{ left: `${variants.length > 1 ? sliderIndex / (variants.length - 1) * 100 : 100}%` }} />
                </div>
                <input className="rsm-native-slider" type="range" min={0} max={Math.max(0, variants.length - 1)} step="any" value={sliderIndex}
                  aria-label="推理等级" aria-valuetext={formatEffort(variants[Math.round(sliderIndex)]?.id ?? '默认')} aria-busy={pending} disabled={variants.length < 2}
                  onChange={event => setPreviewIndex(Number(event.currentTarget.value))}
                  onPointerDown={event => { dragging.current = true; event.currentTarget.setPointerCapture(event.pointerId) }}
                  onPointerCancel={() => { dragging.current = false; setPreviewIndex(null) }}
                  onPointerUp={event => { dragging.current = false; void chooseEffort(Number(event.currentTarget.value)) }}
                  onKeyDown={event => {
                    const delta = ['ArrowRight', 'ArrowUp', 'PageUp'].includes(event.key) ? 1 : ['ArrowLeft', 'ArrowDown', 'PageDown'].includes(event.key) ? -1 : 0
                    if (!delta && event.key !== 'Home' && event.key !== 'End') return
                    event.preventDefault()
                    const next = event.key === 'Home' ? 0 : event.key === 'End' ? variants.length - 1 : Math.max(0, Math.min(variants.length - 1, Math.round(sliderIndex) + delta))
                    void chooseEffort(next)
                  }}
                  onBlur={() => { if (dragging.current) { dragging.current = false; void chooseEffort(sliderIndex) } }}
                />
              </div>
            </div>
          )}


          {error !== undefined && (
            <p className="rsm-error" role="alert">
              {error.message}
            </p>
          )}
        </div>
    </div>
  )
}

function parseSelector(selector: string | null): SelectorParts | null {
  if (selector === null) return null
  const slash = selector.indexOf('/')
  if (slash <= 0 || slash === selector.length - 1) return null
  const hash = selector.lastIndexOf('#')
  return {
    providerId: selector.slice(0, slash),
    modelId: selector.slice(slash + 1, hash > slash ? hash : undefined),
    effort: hash > slash ? selector.slice(hash + 1) || null : null,
  }
}

function composeSelector(providerId: string, modelId: string, effort: string | null): string {
  return `${providerId}/${modelId}${effort === null ? '' : `#${effort}`}`
}

function formatEffort(effort: string): string {
  return effort.charAt(0).toUpperCase() + effort.slice(1)
}
