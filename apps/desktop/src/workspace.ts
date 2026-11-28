export function normalizePath(path: string): string {
  return path.replaceAll('\\', '/')
}

export function workspaceKey(root: string): string {
  return root.toLowerCase()
}

export function workspaceRelativePath(path: string, cwd: string): string | null {
  const root = normalizePath(cwd).replace(/\/+$/, '')
  let normalized = normalizePath(path).trim()
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

export function pathBaseName(path: string): string {
  const trimmed = normalizePath(path).replace(/\/+$/, '')
  const slash = trimmed.lastIndexOf('/')
  return (slash >= 0 ? trimmed.slice(slash + 1) : trimmed) || path
}

export function splitPathNameDir(path: string): { name: string; dir: string } {
  const normalized = normalizePath(path)
  const slash = normalized.lastIndexOf('/')
  return slash >= 0
    ? { name: normalized.slice(slash + 1), dir: normalized.slice(0, slash) }
    : { name: normalized, dir: '' }
}

export function pathExtension(path: string): string {
  return /\.([\w-]+)$/.exec(path)?.[1]?.toLowerCase() ?? ''
}

export function directoryOf(path: string): string {
  return path.replace(/[\\/][^\\/]*$/, '') || path
}

export function displayPath(path: string, cwd: string, userHome?: string | null): string {
  const normalized = normalizePath(path)
  const root = normalizePath(cwd).replace(/\/+$/, '')
  if (normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)) return normalized.slice(root.length + 1)
  const home = userHome ? normalizePath(userHome).replace(/\/+$/, '') : undefined
  if (home && normalized.toLowerCase() === home.toLowerCase()) return '~'
  if (home && normalized.toLowerCase().startsWith(`${home.toLowerCase()}/`)) return `~${normalized.slice(home.length)}`
  return path
}

export function resolveFileUrl(path: string, cwd: string, userHome?: string | null): URL | null {
  let target = normalizePath(path.trim())
  if (target === '') return null
  if (target.startsWith('~/') && userHome) {
    target = `${normalizePath(userHome).replace(/\/+$/, '')}/${target.slice(2)}`
  }
  const directory = normalizePath(cwd).replace(/\/+$/, '')
  const encodePath = (value: string) => encodeURI(value).replace(/[?#]/g, encodeURIComponent)
  try {
    const base = new URL(encodePath(directory.startsWith('//') ? `file:${directory}/` : `file:///${directory}/`))
    const targetHref = /^[a-z]:\//i.test(target) ? `file:///${target}`
      : target.startsWith('//') ? `file:${target}` : target
    return new URL(encodePath(targetHref), base)
  } catch {
    return null
  }
}
