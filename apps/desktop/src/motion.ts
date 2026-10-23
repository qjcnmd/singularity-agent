/**
 * 展开/收起的时间常量。CSS 的 `.disclosure`（见 `styles/surfaces.css`）驱动面板高度，
 * 这里同时供需要用 JS 驱动过渡的组件使用。导出的四个 CSS 变量在启动时写入
 * `documentElement`。
 */
const easeOut = [0.22, 1, 0.36, 1] as const
const expand = { duration: 0.25, ease: easeOut } as const

export const actionToastTransition = { duration: 0.2, ease: easeOut } as const

const collapse = { duration: 0.2, ease: [0.4, 0, 0.2, 1] } as const

export const disclosureCssVariables = {
  '--disclosure-duration': `${expand.duration * 1000}ms`,
  '--disclosure-ease': `cubic-bezier(${expand.ease.join(', ')})`,
  '--disclosure-collapse-duration': `${collapse.duration * 1000}ms`,
  '--disclosure-collapse-ease': `cubic-bezier(${collapse.ease.join(', ')})`,
}

export const disclosureTransition = (expanded: boolean, reducedMotion: boolean | null) => {
  const timing = expanded ? expand : collapse
  return { duration: reducedMotion ? 0 : timing.duration, ease: timing.ease }
}
