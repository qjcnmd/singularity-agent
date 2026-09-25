/** 动作来源键由发起方与原位反馈共用。 */
export const actionOrigin = {
  session: (id: string | null) => `session:${id}`,
  workspace: (id: string | null) => `workspace:${id}`,
  control: (sessionId: string | null, controlId: string) => `control:${sessionId}:${controlId}`,
  provider: (id: string) => `provider:${id}`,
  providerKey: (id: string) => `provider-key:${id}`,
  directoryPicker: 'directory:picker',
}

const inlineActionPrefixes = [actionOrigin.control('', ''), actionOrigin.provider(''), actionOrigin.providerKey('')]
  .map(key => key.slice(0, key.indexOf(':') + 1))

/** 这些动作在对应控件显示错误；目录选择器的错误仍显示在工作台。 */
export function hasInlineActionError(origin: string): boolean {
  return origin !== actionOrigin.directoryPicker && inlineActionPrefixes.some(prefix => origin.startsWith(prefix))
}

/** 待处理动作键：方法名与来源。查询方按同一规则在已订阅的 pendingActions
 *  上查自己的键，不再为了读 pending 去碰全局 store。 */
export function pendingKey(method: string, origin?: string): string {
  return [method, origin].filter((value) => value !== undefined && value !== '').join(':')
}
