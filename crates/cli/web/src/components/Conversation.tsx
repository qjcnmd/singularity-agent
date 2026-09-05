import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { useSelectionGuard } from '../interactions'
import { workbenchStore, type WorkbenchState } from '../store'
import type { TimelineItemModel } from '../timeline'
import { TimelineItem } from './TimelineItem'

interface Props {
  state: WorkbenchState
  items: TimelineItemModel[]
}

export function Conversation({ state, items }: Props) {
  const rows = useMemo(() => groupTimeline(items), [items])
  const itemIdentity = useMemo(() => items.map((item) => item.key).join('\u0000'), [items])
  const viewport = useRef<HTMLDivElement>(null)
  const previousKeys = useRef<Set<string>>(new Set())
  const sessionId = state.selectedSessionId
  const anchor = workbenchStore.viewportAnchor()
  const change = state.timelineChange?.sessionId === sessionId ? state.timelineChange : null

  useLayoutEffect(() => {
    const node = viewport.current
    if (node === null || sessionId === null) return
    const currentKeys = new Set(items.map((item) => item.key))
    const introduced = [...currentKeys].filter((key) => !previousKeys.current.has(key)).length
    previousKeys.current = currentKeys
    const currentAnchor = workbenchStore.viewportAnchor()

    const restoreViewport = () => {
      const latest = workbenchStore.viewportAnchor()
      if (latest.mode === 'following') {
        node.scrollTop = node.scrollHeight
        return
      }
      if (latest.anchorItemId === null) return
      const element = node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(latest.anchorItemId)}"]`)
      if (element !== null) node.scrollTop = element.offsetTop - latest.offset
    }

    restoreViewport()
    if (currentAnchor.mode === 'anchored' && change?.kind === 'append' && introduced > 0) {
      workbenchStore.setViewportAnchor({ ...currentAnchor, unseenCount: currentAnchor.unseenCount + introduced })
    }

    const observer = new ResizeObserver(() => requestAnimationFrame(restoreViewport))
    const document = node.querySelector<HTMLElement>('.conversation-document')
    if (document !== null) observer.observe(document)
    return () => observer.disconnect()
  }, [change?.kind, change?.version, itemIdentity, sessionId])

  const onScroll = (userScroll = false) => {
    const node = viewport.current
    if (node === null) return
    const anchor = workbenchStore.viewportAnchor()
    const distance = node.scrollHeight - node.clientHeight - node.scrollTop
    if (distance < 32 && !userScroll) {
      if (anchor.mode !== 'following' || anchor.unseenCount !== 0) workbenchStore.setViewportAnchor(defaultAnchor)
      return
    }
    // Content reflow also fires scroll events; only user input leaves following mode.
    if (!userScroll && anchor.mode === 'following') return
    const visible = [...node.querySelectorAll<HTMLElement>('[data-item-id]')]
      .find((item) => item.offsetTop + item.offsetHeight >= node.scrollTop)
    if (visible !== undefined) {
      workbenchStore.setViewportAnchor({
        mode: 'anchored',
        anchorItemId: visible.dataset.itemId ?? null,
        offset: visible.offsetTop - node.scrollTop,
        unseenCount: anchor.unseenCount,
      })
    }
  }

  const backToLatest = () => {
    workbenchStore.setViewportAnchor(defaultAnchor)
    const node = viewport.current
    if (node !== null) requestAnimationFrame(() => { node.scrollTop = node.scrollHeight })
  }

  if (state.selectedWorkspaceId === null) {
    return <Empty title="打开一个本地项目" body="选择文件夹后，Singularity 会在项目上下文中读取文件并执行任务。" />
  }
  if (sessionId === null) {
    return <Empty title="准备做什么？" body="从左侧新建任务。每个任务都有独立、可恢复的会话记录。" />
  }
  if (state.session === null) {
    if (state.sessionLoad.status === 'error' && state.sessionLoad.error !== null) {
      return (
        <Empty title="会话读取失败" body={state.sessionLoad.error.message}>
          <p className="empty-recovery">{state.sessionLoad.error.recovery}</p>
          <button type="button" className="secondary-button" onClick={() => workbenchStore.retrySession()}>重试读取</button>
        </Empty>
      )
    }
    return <Empty title="正在读取会话" body="正在加载持久记录和当前运行状态…" busy />
  }

  const sessionOrigin = `session:${sessionId}`
  return (
    <div className="conversation-scroll" ref={viewport} onScroll={() => onScroll()}
      onWheel={(event) => { if (event.deltaY < 0) onScroll(true) }}
      onKeyDown={(event) => {
        if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) onScroll(true)
      }}
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) onScroll(true)
      }}
    >
      <div className="conversation-document">
        {state.session.history.nextCursor !== null && (
          <button
            type="button"
            className="load-older"
            disabled={workbenchStore.isPending('history.older', sessionOrigin)}
            onClick={() => void workbenchStore.readOlder()}
          >
            {workbenchStore.isPending('history.older', sessionOrigin) ? '正在读取…' : '加载更早的记录'}
          </button>
        )}
        {items.length === 0 ? (
          <Empty title="随时可以开始" body="描述完整目标，或先问一个有关项目的问题。输入 @ 可以引用项目文件。" />
        ) : rows.map((row) => row.type === 'item' ? (
          <TimelineItem
            key={row.item.key}
            item={row.item}
            selected={state.detailsItemId === row.item.key}
            onSelect={(selected) => workbenchStore.selectDetails(selected.key)}
          />
        ) : (
          <ActivityGroup key={row.key} items={row.items} selectedItemId={state.detailsItemId} />
        ))}
      </div>
      {anchor.mode === 'anchored' && anchor.unseenCount > 0 && (
        <button type="button" className="new-items-button" onClick={backToLatest}>
          {anchor.unseenCount} 条新动态 · 回到最新
        </button>
      )}
    </div>
  )
}

type TimelineRow =
  | { type: 'item'; item: TimelineItemModel }
  | { type: 'activity'; key: string; items: TimelineItemModel[] }

function groupTimeline(items: TimelineItemModel[]): TimelineRow[] {
  const rows: TimelineRow[] = []
  let activity: TimelineItemModel[] = []
  const flush = () => {
    if (activity.length === 0) return
    rows.push({ type: 'activity', key: `activity:${activity[0].key}`, items: activity })
    activity = []
  }
  for (const item of items) {
    if (item.kind === 'user' || item.kind === 'assistant' || item.kind === 'terminal') {
      flush()
      rows.push({ type: 'item', item })
    } else {
      activity.push(item)
    }
  }
  flush()
  return rows
}

function ActivityGroup({ items, selectedItemId }: { items: TimelineItemModel[]; selectedItemId: string | null }) {
  const live = items.some((item) => item.status === 'running')
  const failedCount = items.filter((item) => item.status === 'failed').length
  const [expanded, setExpanded] = useState(live)
  const selectionGuard = useSelectionGuard()
  const toolCount = items.filter((item) => item.kind === 'tool' || item.kind === 'diff').length
  const summary = [
    `${items.length} 个步骤`,
    toolCount > 0 ? `${toolCount} 次工具调用` : null,
    failedCount > 0 ? `${failedCount} 次失败` : null,
  ].filter(Boolean).join(' · ')
  return (
    <section className={`activity-group${expanded ? ' is-expanded' : ''}${live ? ' is-live' : ''}${failedCount > 0 ? ' has-failure' : ''}`}>
      <button type="button" className="activity-group-toggle" {...selectionGuard(() => setExpanded((value) => !value))} aria-expanded={expanded}>
        <span className="group-mark" aria-hidden="true">{live ? '·' : failedCount > 0 ? '!' : '✓'}</span>
        <strong>{live ? '正在执行' : '执行记录'}</strong>
        <span>{summary}</span>
        <span className="group-chevron" aria-hidden="true">{expanded ? '⌃' : '⌄'}</span>
      </button>
      {expanded && (
        <div className="activity-group-items">
          {items.map((item) => (
            <TimelineItem
              key={item.key}
              item={item}
              selected={selectedItemId === item.key}
              onSelect={(selected) => workbenchStore.selectDetails(selected.key)}
            />
          ))}
        </div>
      )}
    </section>
  )
}

function Empty({ title, body, busy = false, children }: { title: string; body: string; busy?: boolean; children?: ReactNode }) {
  return (
    <section className="empty-state" aria-live="polite">
      {busy ? <span className="spinner" aria-hidden="true" /> : <span className="empty-mark" aria-hidden="true">S</span>}
      <h2>{title}</h2>
      <p>{body}</p>
      {children}
    </section>
  )
}

const defaultAnchor = {
  mode: 'following' as const,
  anchorItemId: null,
  offset: 0,
  unseenCount: 0,
}
