import { useState, type FormEvent } from 'react'
import { workbenchStore, type ActionError, type WorkbenchState } from '../store'
import type { ProviderConfigurationInput, RedactedProvider } from '../protocol'
import { Dialog } from './Dialog'

export function Settings({ state }: { state: WorkbenchState }) {
  const catalog = state.bootstrap?.modelCatalog
  return (
    <Dialog open={state.settingsOpen} onClose={() => workbenchStore.setSettingsOpen(false)} labelledBy="settings-title" className="settings-modal">
      <header className="modal-header">
        <div><span className="eyebrow">设置</span><h2 id="settings-title">模型连接</h2></div>
        <button type="button" className="icon-button" data-autofocus onClick={() => workbenchStore.setSettingsOpen(false)} aria-label="关闭">×</button>
      </header>
      <div className={`configuration-status configuration-${catalog?.configuration ?? 'missing'}`} role="status">
        <strong>{catalog?.configuration === 'ready' ? '模型连接可用' : catalog?.configuration === 'invalid' ? '配置需要处理' : '添加一个模型供应商'}</strong>
        <span>{catalog?.message ?? '连接信息会显示在这里；API 密钥只允许写入，不会返回浏览器。'}</span>
      </div>
      {(catalog?.providers.length ?? 0) > 0 && (
        <div className="provider-list">
          {catalog?.providers.map((provider) => (
            <ProviderCard key={provider.providerId} provider={provider} state={state} />
          ))}
        </div>
      )}
      <ProviderForm state={state} />
    </Dialog>
  )
}

function ProviderCard({ provider, state }: { provider: RedactedProvider; state: WorkbenchState }) {
  const [apiKey, setApiKey] = useState('')
  const keyOrigin = `provider-key:${provider.providerId}`
  const keyPending = workbenchStore.isPending('model.setApiKey', keyOrigin)
  const keyError = state.actionErrors[keyOrigin]

  const submitKey = async (event: FormEvent) => {
    event.preventDefault()
    if (apiKey.trim() === '') return
    const saved = await workbenchStore.setApiKey(provider.providerId, apiKey)
    if (saved) setApiKey('')
  }

  return (
    <article className="provider-card">
      <header>
        <div><strong>{provider.providerId}</strong><span>{provider.baseUrl}</span></div>
        <span className={provider.credentialConfigured ? 'credential-ready' : 'credential-missing'}>
          {provider.credentialConfigured ? '密钥已配置' : '需要密钥'}
        </span>
      </header>
      <div className="provider-models">
        {provider.models.map((model) => (
          <div key={model.modelId}>
            <strong>{model.modelId}</strong>
            <small>{protocolLabel(model.apiProtocol)} · {model.maxContextTokens?.toLocaleString() ?? '默认'} 上下文</small>
          </div>
        ))}
      </div>
      <form className="credential-form" onSubmit={(event) => void submitKey(event)}>
        <label>
          <span>{provider.credentialConfigured ? '替换 API 密钥' : 'API 密钥'} <small>只写</small></span>
          <input type="password" autoComplete="new-password" value={apiKey} onChange={(event) => setApiKey(event.target.value)} required />
        </label>
        <button type="submit" className="primary-button" disabled={keyPending || apiKey.trim() === ''}>
          {keyPending ? '正在写入…' : provider.credentialConfigured ? '替换密钥' : '写入密钥'}
        </button>
      </form>
      {keyError !== undefined && <InlineError error={keyError} />}
    </article>
  )
}

function ProviderForm({ state }: { state: WorkbenchState }) {
  const [providerId, setProviderId] = useState('')
  const [baseUrl, setBaseUrl] = useState('')
  const [modelId, setModelId] = useState('')
  const [protocol, setProtocol] = useState<'chat' | 'responses'>('chat')
  const [context, setContext] = useState('')
  const [output, setOutput] = useState('')
  const [validation, setValidation] = useState<string | null>(null)
  const origin = providerId.trim() === '' ? undefined : `provider:${providerId.trim()}`
  const pending = origin !== undefined && workbenchStore.isPending('model.saveProvider', origin)
  const error = origin === undefined ? undefined : state.actionErrors[origin]

  const save = async (event: FormEvent) => {
    event.preventDefault()
    const issue = validateProvider(providerId, baseUrl, modelId, context, output)
    setValidation(issue)
    if (issue !== null) return
    const provider: ProviderConfigurationInput = {
      providerId: providerId.trim(),
      baseUrl: baseUrl.trim().replace(/\/$/, ''),
      models: [{
        modelId: modelId.trim(),
        apiProtocol: protocol,
        maxContextTokens: optionalPositiveNumber(context),
        maxOutputTokens: optionalPositiveNumber(output),
        reasoningVariants: [],
        defaultVariant: null,
        toolReasoningHistory: null,
      }],
      makeDefault: true,
    }
    await workbenchStore.saveProvider(provider)
  }

  return (
    <form className="provider-form" onSubmit={(event) => void save(event)} noValidate>
      <header>
        <div><span className="eyebrow">连接配置</span><h3>添加或更新供应商</h3></div>
        <p>这里保存模型元数据，并把它设为默认选择。密钥请在保存后到上方对应卡片单独写入。</p>
      </header>
      <div className="form-grid">
        <label><span>供应商 ID</span><input value={providerId} onChange={(event) => { setProviderId(event.target.value); setValidation(null) }} placeholder="例如 mimo" required /></label>
        <label className="span-two"><span>API 基础地址</span><input type="url" value={baseUrl} onChange={(event) => { setBaseUrl(event.target.value); setValidation(null) }} placeholder="https://example.com/v1" required /></label>
        <label><span>模型 ID</span><input value={modelId} onChange={(event) => { setModelId(event.target.value); setValidation(null) }} placeholder="例如 model-name" required /></label>
        <label><span>API 协议</span><select value={protocol} onChange={(event) => setProtocol(event.target.value as 'chat' | 'responses')}><option value="chat">Chat Completions</option><option value="responses">Responses</option></select></label>
        <label><span>上下文 Token <small>可留空</small></span><input inputMode="numeric" value={context} onChange={(event) => { setContext(event.target.value); setValidation(null) }} /></label>
        <label><span>输出 Token <small>可留空</small></span><input inputMode="numeric" value={output} onChange={(event) => { setOutput(event.target.value); setValidation(null) }} /></label>
      </div>
      {validation !== null && <div className="form-error" role="alert">{validation}</div>}
      {error !== undefined && <InlineError error={error} />}
      <footer><button type="submit" className="primary-button" disabled={pending}>{pending ? '正在保存…' : '保存供应商'}</button></footer>
    </form>
  )
}

function InlineError({ error }: { error: ActionError }) {
  return <div className="inline-error" role="alert"><strong>{error.message}</strong><span>{error.recovery}</span></div>
}

function validateProvider(providerId: string, baseUrl: string, modelId: string, context: string, output: string): string | null {
  if (!identifier(providerId)) return '供应商 ID 必须是一个不含空格的名称。'
  if (!identifier(modelId)) return '模型 ID 必须是一个不含空格的名称。'
  try {
    const url = new URL(baseUrl.trim())
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return 'API 基础地址必须使用 http 或 https。'
  } catch {
    return '请输入完整有效的 API 基础地址。'
  }
  if (!optionalPositiveInteger(context)) return '上下文 Token 必须留空或填写正整数。'
  if (!optionalPositiveInteger(output)) return '输出 Token 必须留空或填写正整数。'
  return null
}

function identifier(value: string): boolean {
  return value.trim() !== '' && !/\s/.test(value)
}

function optionalPositiveInteger(value: string): boolean {
  return value.trim() === '' || (Number.isInteger(Number(value)) && Number(value) > 0)
}

function optionalPositiveNumber(value: string): number | null {
  return value.trim() === '' ? null : Number(value)
}

function protocolLabel(protocol: string): string {
  return protocol === 'chat' ? 'Chat Completions' : protocol === 'responses' ? 'Responses' : protocol
}
