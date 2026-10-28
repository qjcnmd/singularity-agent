import type { ModelSelection, ImageUpload } from './protocol'

/** files 是附件文件的绝对路径，发送时只把路径交给后端；disabledModules 是新任务关闭的上下文模块 ID，只在空白页使用。 */
export interface Draft { text: string; images: File[]; files: string[]; selection?: ModelSelection; disabledModules?: string[] }
export const emptyDraft: Draft = { text: '', images: [], files: [] }
const hasDraft = (draft: Draft | undefined) => draft !== undefined && (draft.text !== '' || draft.images.length > 0 || draft.files.length > 0 || draft.selection !== undefined || (draft.disabledModules?.length ?? 0) > 0)

export type DraftSnapshot = Readonly<Record<string, Draft>>

/** 管理各任务的输入草稿（文字、图片、附件和未启动任务的模型选择），同步到 IndexedDB。 */
export class DraftStore {
  private snapshot: DraftSnapshot | null = null
  private loading: Promise<DraftSnapshot> | null = null

  constructor(private readonly onChange: (drafts: DraftSnapshot) => void) {}

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

  /** 编辑立即生效；自动保存失败不影响当前输入和发送。 */
  async set(sessionId: string, draft: Draft): Promise<boolean> {
    if (this.snapshot === null) return false
    const drafts = { ...this.snapshot }
    if (hasDraft(draft)) drafts[sessionId] = draft
    else delete drafts[sessionId]
    this.publish(drafts)
    await persistDraft(sessionId, draft).catch(() => {})
    return true
  }

  /** 发送期间继续编辑的内容仍保留。 */
  async clearIfUnchanged(sessionId: string, submitted: Draft): Promise<void> {
    if (this.snapshot?.[sessionId] === submitted) await this.set(sessionId, emptyDraft)
  }

  /** 清理失败不影响已完成的任务删除或项目移除。 */
  async remove(sessionIds: string[]): Promise<void> {
    await removeDrafts(sessionIds).catch(() => {})
    if (this.snapshot !== null) {
      const drafts = { ...this.snapshot }
      for (const id of sessionIds) delete drafts[id]
      this.publish(drafts)
    }
  }

  private publish(drafts: DraftSnapshot): void {
    this.snapshot = drafts
    this.onChange(drafts)
  }
}

// 每个任务的完整输入作为一条记录保存。图片不占用 localStorage 的小容量配额。
let database: Promise<IDBDatabase> | undefined
function openDatabase(): Promise<IDBDatabase> {
  return database ??= new Promise((resolve, reject) => {
    const request = indexedDB.open('singularity.drafts', 1)
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
    // 加入附件之前保存的草稿没有 files 字段。
    result[String(cursor.key)] = { ...cursor.value as Draft, files: (cursor.value as Partial<Draft>).files ?? [] }
    cursor.continue()
  }
  await complete(transaction)
  return result
}

/** 发送时才把图片编码为 Data URL；草稿阶段只存 `File` 对象，避免每次输入文字都复制大段 Base64。 */
export function imageUpload(file: File): Promise<ImageUpload> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve({ name: file.name, dataUrl: String(reader.result) })
    reader.onerror = () => reject(new Error(`无法读取 ${file.name}：${reader.error?.message}`))
    reader.readAsDataURL(file)
  })
}
