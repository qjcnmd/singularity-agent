import { useCallback, useRef, useState } from 'react'
import { formatElapsed, uiText } from '../copy'
import { useSelectionGuard, useTransientFocus } from '../interactions'
import type { TimelineItemModel } from '../timeline'
import { SectionList } from './TimelineItem'

export function Details({ item, onClose }: { item: TimelineItemModel | null; onClose: () => void }) {
  const panel = useRef<HTMLElement>(null)
  const close = useCallback(onClose, [onClose])
  const [copied, setCopied] = useState(false)
  const selectionGuard = useSelectionGuard()
  useTransientFocus(true, close, panel)

  const copy = async () => {
    if (item === null) return
    await navigator.clipboard.writeText(item.detail)
    setCopied(true)
    window.setTimeout(() => setCopied(false), 1_200)
  }

  return (
    <aside ref={panel} className="details-panel" aria-label="活动详情">
      <header className="details-header">
        <div>
          <span className="panel-label">活动详情</span>
          <h2>{item?.title ?? '尚未选择记录'}</h2>
        </div>
        <button type="button" className="icon-button" data-autofocus {...selectionGuard(close)} aria-label="关闭详情">×</button>
      </header>
      {item === null ? (
        <div className="details-empty"><span aria-hidden="true">▤</span><p>在会话中选择工具、变更、诊断或投递记录查看完整内容。</p></div>
      ) : (
        <div className="details-content">
          <dl className="detail-facts">
            <div><dt>类型</dt><dd>{kindLabel(item.kind)}</dd></div>
            <div><dt>状态</dt><dd>{statusLabel(item.status)}</dd></div>
            <div><dt>行数</dt><dd>{item.detail.split('\n').length}</dd></div>
          </dl>
          {(item.filePath !== null || item.durationMs !== null || item.addedLines > 0 || item.removedLines > 0) && (
            <div className="detail-summary">
              {item.filePath !== null && <code title={item.filePath}>{item.filePath}</code>}
              {item.durationMs !== null && <span>耗时 {formatElapsed(item.status === 'running' ? item.startedAt : new Date(Date.now() - item.durationMs).toISOString())}</span>}
              {(item.addedLines > 0 || item.removedLines > 0) && <span className="detail-diff-stat"><b>+{item.addedLines}</b><i>−{item.removedLines}</i></span>}
            </div>
          )}
          <SectionList sections={item.sections} fallback={item.detail || '没有额外输出。'} />
          <button type="button" className="secondary-button copy-detail" {...selectionGuard(() => { void copy() })}>
            {copied ? uiText.copied : '复制完整内容'}
          </button>
        </div>
      )}
    </aside>
  )
}

function kindLabel(kind: TimelineItemModel['kind']): string {
  return ({
    user: '用户消息',
    assistant: 'Agent 消息',
    thinking: '思考',
    tool: '工具',
    diff: '文件变更',
    diagnostic: '诊断',
    control: '投递记录',
    terminal: '终态',
    unknown: '原始事件',
  } as const)[kind]
}

function statusLabel(status: TimelineItemModel['status']): string {
  return ({ stable: '已记录', running: '进行中', completed: '已完成', failed: '失败', interrupted: '已停止', pending: '等待中' } as const)[status]
}
