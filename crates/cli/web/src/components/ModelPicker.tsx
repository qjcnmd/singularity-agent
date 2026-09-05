import { useEffect, useMemo, useRef, useState } from 'react'
import { useSelectionGuard, useTransientFocus } from '../interactions'
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

export function ModelPicker({
  state,
  open,
  onOpenChange,
}: {
  state: WorkbenchState
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const root = useRef<HTMLDivElement>(null)
  const catalog = state.bootstrap?.modelCatalog
  const selector = state.session?.runtime.selector ?? catalog?.defaultSelector ?? null
  const parsed = parseSelector(selector)
  const choices = useMemo(
    () => catalog?.providers.flatMap((provider) => provider.models.map((model) => ({ provider, model }))) ?? [],
    [catalog?.providers],
  )
  const currentChoice = choices.find(({ provider, model }) => provider.providerId === parsed?.providerId && model.modelId === parsed.modelId)
  const variants = currentChoice?.model.reasoningVariants.filter((variant) => variant.enabled) ?? []
  const resolvedEffort = parsed?.effort ?? currentChoice?.model.defaultVariant ?? variants[0]?.id ?? null
  const resolvedIndex = Math.max(0, variants.findIndex((variant) => variant.id === resolvedEffort))
  const [previewIndex, setPreviewIndex] = useState(resolvedIndex)
  const selectionGuard = useSelectionGuard()
  const sessionId = state.selectedSessionId
  const origin = sessionId === null ? undefined : `session:${sessionId}`
  const pending = workbenchStore.isPending('session.updateSettings', origin)
  const feedback = sessionId === null ? undefined : state.settingsFeedback[sessionId]
  const error = origin === undefined ? undefined : state.actionErrors[origin]

  useEffect(() => setPreviewIndex(resolvedIndex), [resolvedIndex, selector])
  useTransientFocus(open, () => onOpenChange(false), root)
  useEffect(() => {
    if (!open) return
    const close = (event: PointerEvent) => {
      if (event.target instanceof Node && !root.current?.contains(event.target)) onOpenChange(false)
    }
    document.addEventListener('pointerdown', close)
    return () => document.removeEventListener('pointerdown', close)
  }, [onOpenChange, open])

  const chooseModel = async (choice: ModelChoice) => {
    const enabled = choice.model.reasoningVariants.filter((variant) => variant.enabled)
    const effort = enabled.some((variant) => variant.id === resolvedEffort)
      ? resolvedEffort
      : choice.model.defaultVariant ?? enabled[0]?.id ?? null
    await workbenchStore.updateSettings(composeSelector(choice.provider.providerId, choice.model.modelId, effort))
  }

  const chooseEffort = async (index: number) => {
    const variant = variants[index]
    if (variant === undefined || currentChoice === undefined || variant.id === resolvedEffort) return
    await workbenchStore.updateSettings(composeSelector(currentChoice.provider.providerId, currentChoice.model.modelId, variant.id))
  }

  const modelLabel = currentChoice?.model.modelId ?? parsed?.modelId ?? '选择模型'
  const effortLabel = resolvedEffort === null ? null : formatEffort(resolvedEffort)

  return (
    <div className="model-picker" ref={root}>
      <button
        type="button"
        className="model-trigger"
        disabled={sessionId === null}
        aria-haspopup="dialog"
        aria-expanded={open}
        {...selectionGuard(() => onOpenChange(!open))}
      >
        <span>{modelLabel}</span>
        {effortLabel !== null && <span className="model-effort">{effortLabel}</span>}
        <span className="chevron" aria-hidden="true">⌄</span>
      </button>
      {open && (
        <div className="model-popover" role="dialog" aria-label="模型与思考程度">
          <header className="model-popover-header">
            <strong>模型</strong>
            <button
              type="button"
              className="quiet-button"
              {...selectionGuard(() => {
                onOpenChange(false)
                workbenchStore.setSettingsOpen(true)
              })}
            >
              连接设置
            </button>
          </header>
          <div className="model-list" role="listbox" aria-label="可用模型">
            {catalog?.providers.map((provider) => (
              <section className="model-provider-group" key={provider.providerId}>
                <span className="model-provider-name">{provider.providerId}</span>
                {provider.models.map((model) => {
                  const selected = provider.providerId === currentChoice?.provider.providerId && model.modelId === currentChoice.model.modelId
                  return (
                    <button
                      type="button"
                      role="option"
                      aria-selected={selected}
                      disabled={!provider.credentialConfigured || pending}
                      key={model.modelId}
                      data-autofocus={selected || undefined}
                      {...selectionGuard(() => { void chooseModel({ provider, model }) })}
                    >
                      <span><strong>{model.modelId}</strong><small>{model.maxContextTokens === null ? '默认上下文' : `${formatTokens(model.maxContextTokens)} 上下文`}</small></span>
                      <span className="model-check" aria-hidden="true">{selected ? '✓' : ''}</span>
                    </button>
                  )
                })}
              </section>
            ))}
          </div>
          {variants.length > 0 && (
            <section className="effort-control">
              <header><span>思考程度</span><strong>{formatEffort(variants[previewIndex]?.id ?? resolvedEffort ?? '')}</strong></header>
              <input
                type="range"
                min={0}
                max={Math.max(0, variants.length - 1)}
                step={1}
                value={previewIndex}
                disabled={pending || variants.length < 2}
                aria-label="思考程度"
                aria-valuetext={formatEffort(variants[previewIndex]?.id ?? '')}
                onChange={(event) => setPreviewIndex(Number(event.target.value))}
                onPointerUp={() => { void chooseEffort(previewIndex) }}
                onKeyUp={(event) => {
                  if (event.key.startsWith('Arrow') || event.key === 'Home' || event.key === 'End') void chooseEffort(previewIndex)
                }}
                onBlur={() => { void chooseEffort(previewIndex) }}
              />
              <div className="effort-labels" aria-hidden="true">
                {variants.map((variant) => <span key={variant.id}>{formatEffort(variant.id)}</span>)}
              </div>
            </section>
          )}
          {feedback !== undefined && <p className="model-feedback">{feedback.applyTiming === 'next_turn' ? '已保存，将从下一回合生效。' : '已保存。'}</p>}
          {error !== undefined && <p className="model-error" role="alert"><strong>{error.message}</strong><span>{error.recovery}</span></p>}
        </div>
      )}
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
  const labels: Record<string, string> = {
    none: '关闭',
    off: '关闭',
    minimal: '最低',
    low: '低',
    medium: '中',
    high: '高',
    xhigh: '极高',
    max: '最高',
    ultra: '超高',
  }
  return labels[effort.toLowerCase()] ?? effort
}

function formatTokens(tokens: number): string {
  return tokens >= 1_000_000 ? `${Number((tokens / 1_000_000).toFixed(1))}M` : `${Math.round(tokens / 1_000)}K`
}
