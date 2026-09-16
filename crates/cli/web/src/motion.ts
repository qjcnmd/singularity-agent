/**
 * 展开/收起的时间常量。CSS 的 disclosure（见 `styles/tokens.css`）驱动面板高度，
 * 这里驱动仍需 JS 计算高度的少数地方；两处若各写各的值，同一次展开在不同位置
 * 就会跑出不同速度。
 */
const expand = { duration: 0.25, ease: [0.22, 1, 0.36, 1] } as const

const collapse = { duration: 0.2, ease: [0.4, 0, 0.2, 1] } as const

/** 按方向选取时序；开启“减少动效”时归零时长。 */
export const disclosureTransition = (expanded: boolean, reducedMotion: boolean | null) => {
  const timing = expanded ? expand : collapse
  return { duration: reducedMotion ? 0 : timing.duration, ease: timing.ease }
}
