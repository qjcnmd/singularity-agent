/**
 * 展开/收起的时间常量，也是 disclosure 时序的唯一权威：CSS 的 disclosure
 * （见 `styles/tokens.css`）驱动面板高度，这里驱动仍需 JS 计算高度的少数地方。
 * 下面导出的四个自定义属性在启动时写入 documentElement，由 CSS 消费同名变量。
 */
const expand = { duration: 0.25, ease: [0.22, 1, 0.36, 1] } as const

const collapse = { duration: 0.2, ease: [0.4, 0, 0.2, 1] } as const

export const disclosureCssVariables = {
  '--disclosure-duration': `${expand.duration * 1000}ms`,
  '--disclosure-ease': `cubic-bezier(${expand.ease.join(', ')})`,
  '--disclosure-collapse-duration': `${collapse.duration * 1000}ms`,
  '--disclosure-collapse-ease': `cubic-bezier(${collapse.ease.join(', ')})`,
}

/** 按方向选取时序；开启“减少动效”时归零时长。 */
export const disclosureTransition = (expanded: boolean, reducedMotion: boolean | null) => {
  const timing = expanded ? expand : collapse
  return { duration: reducedMotion ? 0 : timing.duration, ease: timing.ease }
}
