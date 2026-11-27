export function workspaceKey(root: string): string {
  return root.toLowerCase()
}

export function workspaceRelativePath(path: string, cwd: string): string | null {
  const root = cwd.replaceAll('\\', '/').replace(/\/+$/, '')
  let normalized = path.replaceAll('\\', '/').trim()
  while (normalized.startsWith('./')) normalized = normalized.slice(2)
  if (normalized === '' || normalized === '~' || normalized.startsWith('~/')) return null
  const isAbsolute = /^[a-z]:\//i.test(normalized) || normalized.startsWith('/')
  if (isAbsolute) {
    if (!normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)) return null
    normalized = normalized.slice(root.length + 1)
  }
  if (normalized === '' || normalized === '..' || normalized.startsWith('../') || normalized.includes('/../') || normalized.endsWith('/..')) {
    return null
  }
  return normalized
}
