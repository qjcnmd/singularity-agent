import { DRAFTS_DB_NAME } from './identity'
import type { ModelSelection, ImageUpload } from './protocol'

export interface DraftAnnotation {
  id: string
  messageId: string
  quote: string
  comment: string
  startOffset: number
  endOffset: number
  prefix: string
  suffix: string
}

export interface Draft {
  text: string
  images: File[]
  files: string[]
  annotations: DraftAnnotation[]
  selection?: ModelSelection
  disabledModules?: string[]
}
export const emptyDraft: Draft = { text: '', images: [], files: [], annotations: [] }
export const hasDraftContent = (draft: Pick<Draft, 'text' | 'images' | 'files' | 'annotations'>) =>
  draft.text.trim() !== '' || draft.images.length > 0 || draft.files.length > 0 || draft.annotations.length > 0

const hasDraft = (draft: Draft | undefined) => draft !== undefined && (
  draft.text !== ''
  || draft.images.length > 0
  || draft.files.length > 0
  || draft.annotations.length > 0
  || draft.selection !== undefined
  || (draft.disabledModules?.length ?? 0) > 0
)

export type DraftSnapshot = Readonly<Record<string, Draft>>

export class DraftStore {
  private snapshot: DraftSnapshot | null = null
  private loading: Promise<DraftSnapshot> | null = null
  private pendingSaves = new Map<string, { timer: ReturnType<typeof setTimeout>; draft: Draft }>()

  constructor(private readonly onChange: (drafts: DraftSnapshot) => void) {
    if (typeof window !== 'undefined') {
      window.addEventListener('pagehide', () => this.flushAll())
    }
  }

  load(): Promise<DraftSnapshot> {
    if (this.snapshot !== null) return Promise.resolve(this.snapshot)
    return this.loading ??= loadDrafts().catch(() => ({})).then(drafts => {
      this.publish(drafts)
      return drafts
    }).finally(() => { this.loading = null })
  }

  get(sessionId: string): Draft {
    return this.snapshot?.[sessionId] ?? emptyDraft
  }

  async set(sessionId: string, draft: Draft): Promise<boolean> {
    if (this.snapshot === null) return false
    const drafts = { ...this.snapshot }
    if (hasDraft(draft)) drafts[sessionId] = draft
    else delete drafts[sessionId]
    this.publish(drafts)
    const previous = this.pendingSaves.get(sessionId)
    if (previous) clearTimeout(previous.timer)
    const timer = setTimeout(() => {
      this.pendingSaves.delete(sessionId)
      void persistDraft(sessionId, draft).catch(() => {})
    }, 250)
    this.pendingSaves.set(sessionId, { timer, draft })
    return true
  }

  async clearIfUnchanged(sessionId: string, submitted: Draft): Promise<void> {
    const snapshot = this.snapshot
    if (snapshot === null) return
    const current = snapshot[sessionId]
    if (current !== submitted) {
      if (!current || submitted.annotations.length === 0) return
      const submittedComments = new Map(submitted.annotations.map(item => [item.id, item.comment]))
      const remaining = current.annotations.filter(item => submittedComments.get(item.id) !== item.comment)
      if (remaining.length === current.annotations.length) return
      const next = { ...current, annotations: remaining }
      if (hasDraft(next)) {
        await this.set(sessionId, next)
        return
      }
    }
    this.cancelPending([sessionId])
    const drafts = { ...snapshot }
    delete drafts[sessionId]
    this.publish(drafts)
    await persistDraft(sessionId, emptyDraft).catch(() => {})
  }

  async remove(sessionIds: string[]): Promise<void> {
    this.cancelPending(sessionIds)
    await removeDrafts(sessionIds).catch(() => {})
    if (this.snapshot !== null) {
      const drafts = { ...this.snapshot }
      for (const id of sessionIds) delete drafts[id]
      this.publish(drafts)
    }
  }

  private cancelPending(sessionIds: string[]): void {
    for (const id of sessionIds) {
      const pending = this.pendingSaves.get(id)
      if (!pending) continue
      clearTimeout(pending.timer)
      this.pendingSaves.delete(id)
    }
  }

  private flushAll(): void {
    for (const [id, pending] of this.pendingSaves) {
      clearTimeout(pending.timer)
      void persistDraft(id, pending.draft).catch(() => {})
    }
    this.pendingSaves.clear()
  }

  private publish(drafts: DraftSnapshot): void {
    this.snapshot = drafts
    this.onChange(drafts)
  }
}

let database: Promise<IDBDatabase> | undefined
function openDatabase(): Promise<IDBDatabase> {
  return database ??= new Promise((resolve, reject) => {
    const request = indexedDB.open(DRAFTS_DB_NAME, 1)
    request.onupgradeneeded = () => request.result.createObjectStore('drafts')
    request.onsuccess = () => resolve(request.result)
    request.onerror = () => { database = undefined; reject(request.error) }
  })
}

function complete(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve()
    transaction.onabort = () => reject(transaction.error)
    transaction.onerror = () => reject(transaction.error)
  })
}

async function persistDraft(id: string, draft: Draft): Promise<void> {
  const transaction = (await openDatabase()).transaction('drafts', 'readwrite')
  const store = transaction.objectStore('drafts')
  if (hasDraft(draft)) store.put(draft, id)
  else store.delete(id)
  await complete(transaction)
}

async function removeDrafts(ids: string[]): Promise<void> {
  const transaction = (await openDatabase()).transaction('drafts', 'readwrite')
  const store = transaction.objectStore('drafts')
  for (const id of ids) store.delete(id)
  await complete(transaction)
}

async function loadDrafts(): Promise<Record<string, Draft>> {
  const transaction = (await openDatabase()).transaction('drafts', 'readonly')
  const result: Record<string, Draft> = {}
  const request = transaction.objectStore('drafts').openCursor()
  request.onsuccess = () => {
    const cursor = request.result
    if (!cursor) return
    const saved = cursor.value as Partial<Draft>
    result[String(cursor.key)] = {
      ...saved as Draft,
      files: saved.files ?? [],
      annotations: saved.annotations ?? [],
    }
    cursor.continue()
  }
  await complete(transaction)
  return result
}

export function imageUpload(file: File): Promise<ImageUpload> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve({ name: file.name, dataUrl: String(reader.result) })
    reader.onerror = () => reject(new Error(`无法读取 ${file.name}：${reader.error?.message}`))
    reader.readAsDataURL(file)
  })
}
