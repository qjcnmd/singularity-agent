import { motion, useReducedMotion } from 'motion/react'
import { MessageSquare, Pencil, Trash2, ArrowUp } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { PendingInput } from '../protocol'
import { useSelectionGuard } from '../interactions'
import { disclosureTransition } from '../motion'
import { AttachedImages } from './Images'

type QueueState = Pick<AppState, 'pendingActions' | 'drafts'>

/** 消息与动作始终绑定同一个会话，退场动画不会改变其归属。 */
export function QueuedInput({ sessionId, control, state, canSend, onEdit }: {
  sessionId: string; control: PendingInput; state: QueueState; canSend: boolean; onEdit: () => void
}) {
  const transition = disclosureTransition(true, useReducedMotion())
  const guard = useSelectionGuard()
  const origin = actionOrigin.session(sessionId)
  const pending = state.pendingActions.has(pendingKey('session.queue', origin))
  return <motion.div className="queued-inputs-motion" initial={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} animate={{ height: 'auto', opacity: 1, y: 0, marginBottom: -8 }} exit={{ height: 0, opacity: 0, y: 12, marginBottom: 0 }} transition={transition}>
    <div className="queued-inputs" aria-label="排队消息"><div className="queue-row">
      <MessageSquare size={16} aria-hidden="true" />
      <div className="queue-input"><span className="queue-text">{control.text}</span><AttachedImages sessionId={sessionId} images={control.images ?? []} /></div>
      <span className="queue-actions">
        <button type="button" aria-label="编辑消息" title="编辑" disabled={pending || state.drafts === null} {...guard(onEdit)}><Pencil size={17} /></button>
        <button type="button" aria-label="删除排队消息" title="删除" disabled={pending} {...guard(() => { void appStore.withdraw(sessionId, control.controlId) })}><Trash2 size={17} /></button>
        <button type="button" aria-label="立即发送排队消息" title="发送" disabled={pending || !canSend} {...guard(() => { void appStore.sendNow(sessionId, control.controlId) })}><ArrowUp size={19} /></button>
      </span>
    </div></div>
  </motion.div>
}
