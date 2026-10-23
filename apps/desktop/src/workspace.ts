/** 后端提供已规范化的目录；分组和界面偏好统一忽略大小写。 */
export function workspaceKey(root: string): string {
  return root.toLowerCase()
}
