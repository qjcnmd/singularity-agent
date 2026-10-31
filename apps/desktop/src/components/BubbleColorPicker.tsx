import { useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { motion, useReducedMotion } from 'motion/react'
import { appStore, useAppStore } from '../appStore'
import { defaultBubbleColors, type PaletteColor } from '../bubbleColors'
import { enter, pop } from '../motion'
import { ColorWheel, type BallLook } from './ColorWheel'

export type BubbleKind = 'assistantBubble' | 'userBubble'

/** “灰黑”对应的是默认的白底细边框，调色盘上画成和其他球同风格的珍珠白球；用户气泡和助手气泡共用同一颗。 */
const neutralLooks: Partial<Record<string, BallLook>> = { neutral: { label: '白色（默认）', ball: '#d5dbe6', rim: '#9aa4b5' } }

type Drop = { color: PaletteColor; fill: string; rim: string; from: { x: number; y: number }; landed: boolean }

/** 在会话气泡上右键时出现在指针旁的调色盘；选中的颜色作用于同一方的所有气泡。
 * 选中后其他球淡出，选中的球像水滴一样落到右键的位置，落下时泛起涟漪、气泡变色，然后关闭。 */
export function BubbleColorPicker({ kind, x, y, onClose }: { kind: BubbleKind; x: number; y: number; onClose: () => void }) {
  const state = useAppStore([kind])
  const reduced = useReducedMotion()
  const root = useRef<HTMLDivElement>(null)
  const [drop, setDrop] = useState<Drop | null>(null)
  useLayoutEffect(() => {
    const panel = root.current!
    panel.showPopover()
    // 放在指针右下方，靠近窗口边缘时收回窗口内。
    const { width, height } = panel.getBoundingClientRect()
    panel.style.left = `${Math.max(8, Math.min(x, innerWidth - width - 8))}px`
    panel.style.top = `${Math.max(8, Math.min(y, innerHeight - height - 8))}px`
    panel.querySelector<HTMLElement>('[tabindex="0"]')?.focus({ preventScroll: true })
    return () => panel.hidePopover()
  }, [x, y])
  const choose = (color: PaletteColor, ball: HTMLElement) => {
    if (reduced) { appStore.setBubbleColor(kind, color.id); root.current!.hidePopover(); return }
    const rect = ball.getBoundingClientRect()
    const look = neutralLooks[color.id]
    setDrop({ color, fill: look?.ball ?? color.solid, rim: look?.rim ?? color.solid, from: { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }, landed: false })
  }
  const land = () => {
    if (!drop || drop.landed) return
    appStore.setBubbleColor(kind, drop.color.id)
    setDrop({ ...drop, landed: true })
  }
  return createPortal(<div ref={root} popover="auto" className="color-popover" onToggle={event => { if (event.newState === 'closed') onClose() }}>
    <motion.div className="color-popover-surface" {...enter} animate={drop ? { opacity: 0, scale: .96 } : enter.animate} transition={reduced ? { duration: 0 } : drop ? { duration: .18 } : pop} style={{ originX: 0, originY: 0 }}>
      <ColorWheel label={kind === 'userBubble' ? '我的消息颜色' : '助手消息颜色'} value={state[kind] ?? defaultBubbleColors[kind]}
        chosen={drop?.color.id ?? null} looks={neutralLooks} onSelect={choose} />
    </motion.div>
    {drop && !drop.landed && <motion.i className="color-drop" style={{ '--ball': drop.fill, '--ball-rim': drop.rim, left: drop.from.x, top: drop.from.y } as React.CSSProperties}
      // 先微微上提，再加速落下；下落时拉长成水滴，落地前压扁。
      animate={{ x: [0, 0, x - drop.from.x], y: [0, -8, y - drop.from.y], scaleX: [1, .92, .78], scaleY: [1, 1.08, 1.35] }}
      transition={{ duration: .5, times: [0, .25, 1], ease: ['easeOut', 'easeIn'] }} onAnimationComplete={land} />}
    {drop?.landed && <motion.i className="color-ripple" style={{ '--ball': drop.rim, left: x, top: y } as React.CSSProperties}
      initial={{ scale: .2, opacity: .7 }} animate={{ scale: 3.2, opacity: 0 }} transition={{ duration: .45, ease: 'easeOut' }}
      onAnimationComplete={() => root.current?.hidePopover()} />}
  </div>, document.body)
}
