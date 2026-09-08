/** Detect the command/file token at the caret, excluding paths and URLs. */
export function inputTrigger(text: string, caret: number): { kind: 'skill' | 'file'; start: number; end: number; query: string } | null {
  for (let start = caret - 1; start >= 0; start--) {
    const marker = text[start]
    if (/\s/u.test(marker)) break
    if (marker !== '/' && marker !== '@') continue
    const previous = text[start - 1]
    if (previous && /[\p{L}\p{N}_]/u.test(previous)) continue
    if (marker === '/' && (previous === '/' || text[start + 1] === '/'
      || previous === ':' && start > 1 && !/\s/u.test(text[start - 2]))) continue
    return { kind: marker === '/' ? 'skill' : 'file', start, end: caret, query: text.slice(start + 1, caret) }
  }
  return null
}

export interface SkillCandidate { name: string; description: string }
export interface SkillCatalog { skills: SkillCandidate[]; diagnostics: string[] }
