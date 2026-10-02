import { useState } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { ImagePlus, MessageSquare, Pencil, Trash2, ArrowUp, Check, X, ChevronDown } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { PendingInput } from '../protocol'
import { imageFile, type Draft } from '../drafts'
import { useSelectionGuard } from '../interactions'
import { disclosureTransition } from '../motion'
import { AttachedImages, DraftImages, useImageInput } from './Images'

/** 队列行只声明自己读取的字段：Composer 按同一份清单订阅。 */
type QueueState = Pick<AppState, 'selectedSessionId' | 'actionErrors' | 'pendingActions'>

export function QueuedInputs({ controls, state }: { controls: PendingInput[]; state: QueueState }) {
  const reducedMotion = useReducedMotion()
  // 队列的进出场与 disclosure 共用同一组时序，避免同为展开却快慢不一。
  const transition = disclosureTransition(true, reducedMotion)
  const [expanded, setExpanded] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  // 被编辑项可能已被后台消费或撤回：只有它仍在队列里才算正在编辑。
  const editing = editingId !== null && controls.some(control => control.controlId === editingId)
  const visible = expanded || editing ? controls : controls.slice(0, 1)
  return <motion.div className="queued-inputs-motion" initial={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} animate={{ height: 'auto', opacity: 1, y: 0, marginBottom: -8 }} exit={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} transition={transition}><div className="queued-inputs" aria-label="排队消息">
    {controls.length > 1 && <button className="queue-toggle" type="button" aria-expanded={expanded || editing} onClick={() => setExpanded(!expanded)}>
      <ChevronDown size={14} />{controls.length} 条排队消息
    </button>}
    <AnimatePresence initial={false}>{visible.map(control => <motion.div key={control.controlId} initial={{ height: 0, opacity: 0, y: 10 }} animate={{ height: 'auto', opacity: 1, y: 0 }} exit={{ height: 0, opacity: 0, y: 10 }} transition={transition} style={{ overflow: 'hidden' }}><QueueRow control={control} state={state}
      editing={editingId === control.controlId} onEdit={value => setEditingId(current =>
        // 关闭编辑只作用于发起操作的那一行：保存是逐行异步的，A 的晚到回调
        // 不能关掉期间已打开的 B。
        value ? control.controlId : current === control.controlId ? null : current
      )} /></motion.div>)}</AnimatePresence>
  </div></motion.div>
}

function QueueRow({ control, state, editing, onEdit }: { control: PendingInput; state: QueueState; editing: boolean; onEdit: (value: boolean) => void }) {
  const [draft, setDraft] = useState<Draft | null>({ text: control.text, images: [] })
  const [imageError, setImageError] = useState<string | null>(null)
  const loading = draft === null && imageError === null
  const imageInput = useImageInput(images => setDraft(current => current && ({ ...current, images: [...current.images, ...images] })), !editing || draft === null)
  const nonempty = draft !== null && (draft.text.trim() !== '' || draft.images.length > 0)
  const startEdit = async () => {
    onEdit(true); setDraft(null); setImageError(null)
    try {
      const images = await Promise.all((control.images ?? []).map(async image => imageFile(image, await appStore.transport.rpc('session.imageRead', { sessionId: state.selectedSessionId!, imageId: image.id }))))
      setDraft({ text: control.text, images })
    } catch (error) { setImageError(error instanceof Error ? error.message : String(error)) }
  }
  const selectionGuard = useSelectionGuard()
  const origin = actionOrigin.control(state.selectedSessionId, control.controlId)
  const pending = ['session.queueReplace', 'session.queueSendNow', 'session.queueWithdraw']
    .some(method => state.pendingActions.has(pendingKey(method, origin)))
  const error = state.actionErrors[origin]
  const save = async () => {
    if (pending || draft === null || !nonempty) return
    if (await appStore.replace(control.controlId, draft)) onEdit(false)
  }
  const cancel = () => { onEdit(false) }
  return <div className="queue-row" {...imageInput.handlers}>
    <MessageSquare size={16} aria-hidden="true" />
    {editing && draft !== null ? <div className="queue-input"><DraftImages images={draft.images} remove={index => setDraft(current => current && ({ ...current, images: current.images.filter((_, at) => at !== index) }))} /><textarea autoFocus value={draft.text} onChange={event => setDraft(current => current && ({ ...current, text: event.target.value }))} aria-label="编辑排队消息"
      onKeyDown={event => {
        if (event.key === 'Escape') { event.preventDefault(); cancel() }
        if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); if (!event.repeat) void save() }
      }} /></div> : <div className="queue-input"><span className="queue-text">{control.text}</span>{state.selectedSessionId && <AttachedImages sessionId={state.selectedSessionId} images={control.images ?? []} />}</div>}
    <span className="queue-actions">
      {editing ? <>
        {imageInput.picker}
        <button type="button" aria-label="添加图片" title="添加图片" disabled={imageInput.disabled} onClick={imageInput.open}><ImagePlus size={17} /></button>
        <button type="button" aria-label="保存消息" title="保存" disabled={pending || !nonempty} {...selectionGuard(() => { void save() })}><Check size={17} /></button>
        <button type="button" aria-label="取消编辑" title="取消编辑" disabled={pending} {...selectionGuard(cancel)}><X size={17} /></button>
      </> : <>
        <button type="button" aria-label="编辑消息" title="编辑" disabled={pending || loading} {...selectionGuard(() => { void startEdit() })}><Pencil size={17} /></button>
        <button type="button" aria-label="删除排队消息" title="删除" disabled={pending} {...selectionGuard(() => { void appStore.withdraw(control.controlId) })}><Trash2 size={17} /></button>
        <button type="button" aria-label="立即发送排队消息" title="立即发送" disabled={pending || !appStore.canSendNow()} {...selectionGuard(() => { void appStore.sendNow(control.controlId) })}><ArrowUp size={19} /></button>
      </>}
    </span>
    {imageError && <p className="queue-error" role="alert">{imageError}</p>}
    {error !== undefined && <div className="queue-error" role="alert"><strong>{error.message}</strong><span>{error.recovery}</span></div>}
  </div>
}
