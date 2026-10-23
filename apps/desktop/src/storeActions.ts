/** 操作来源的键：发起操作的控件和显示错误的位置用同一个键对应起来。 */
export const actionOrigin = {
  session: (id: string | null) => `session:${id}`,
  control: (sessionId: string | null, controlId: string) => `control:${sessionId}:${controlId}`,
  inline: (method: string, id: string | null) => `inline:${method}:${id}`,
  directoryPicker: 'directory:picker',
}

/** 这些动作在对应控件显示错误；目录选择器的错误仍显示在工作台。 */
export function hasInlineActionError(origin: string): boolean {
  return origin.startsWith('control:') || origin.startsWith('inline:')
}

/** 进行中操作的键：方法名加来源。组件用它在已订阅的 pendingActions 里判断自己的操作是否进行中。 */
export function pendingKey(method: string, origin: string): string {
  return `${method}:${origin}`
}
