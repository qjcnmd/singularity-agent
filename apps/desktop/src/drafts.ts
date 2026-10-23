import type { ModelSelection, ImageUpload } from './protocol'

export interface Draft { text: string; images: File[]; selection?: ModelSelection }
export const emptyDraft: Draft = { text: '', images: [] }
const hasDraft = (draft: Draft | undefined) => draft !== undefined && (draft.text !== '' || draft.images.length > 0 || draft.selection !== undefined)

export type DraftSnapshot = Readonly<Record<string, Draft>>

/** IndexedDB 草稿读写失败错误，附带恢复建议。 */
export class DraftStorageError extends Error {
  constructor(message: string, readonly recovery: string) {
    super(message)
    this.name = 'DraftStorageError'
  }
}

/** 管理各任务的输入草稿（文字、图片和未启动任务的模型选择），同步到 IndexedDB。 */
export class DraftStore {
  private snapshot: DraftSnapshot | null = null
  private loading: Promise<DraftSnapshot | null> | null = null

  constructor(
    private readonly onChange: (drafts: DraftSnapshot) => void,
    private readonly onError: (error: DraftStorageError, sessionId: string | null) => void,
  ) {}

  load(): Promise<DraftSnapshot | null> {
    if (this.snapshot !== null) return Promise.resolve(this.snapshot)
    return this.loading ??= loadDrafts().then(drafts => {
      this.publish(drafts)
      return drafts
    }, error => {
      this.onError(new DraftStorageError(`无法读取草稿：${error instanceof Error ? error.message : String(error)}`, '检查本地存储空间后刷新页面。'), null)
      return null
    }).finally(() => { this.loading = null })
  }

  get(sessionId: string): Draft {
    return this.snapshot?.[sessionId] ?? emptyDraft
  }

  /** 编辑时先更新内存快照；写入 IndexedDB 失败时保留当前页面内的草稿并报错。 */
  async set(sessionId: string, draft: Draft): Promise<boolean> {
    if (this.snapshot === null) return false
    const drafts = { ...this.snapshot }
    if (hasDraft(draft)) drafts[sessionId] = draft
    else delete drafts[sessionId]
    this.publish(drafts)
    try {
      await persistDraft(sessionId, draft)
      return true
    } catch {
      this.onError(new DraftStorageError('草稿暂时只能保留在当前页面。', '请保留页面并检查本地存储空间后重试。'), sessionId)
      return false
    }
  }

  /** 发送期间继续编辑的内容仍保留。 */
  async clearIfUnchanged(sessionId: string, submitted: Draft): Promise<void> {
    if (this.snapshot?.[sessionId] === submitted) await this.set(sessionId, emptyDraft)
  }

  /** 任务或项目删除后清理对应草稿；IndexedDB 删除成功后才更新内存快照。 */
  async remove(sessionIds: string[], completed: string): Promise<void> {
    try {
      await removeDrafts(sessionIds)
    } catch (error) {
      throw new DraftStorageError(`${completed}，但草稿清理失败：${error instanceof Error ? error.message : String(error)}`, '草稿仍保存在本机，请检查本地存储状态。')
    }
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
    result[String(cursor.key)] = cursor.value as Draft
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
