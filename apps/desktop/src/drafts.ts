import type { ImageUpload } from './protocol'

export interface Draft { text: string; images: File[]; skills?: Record<string, string> }
export const emptyDraft: Draft = { text: '', images: [] }
export const hasDraft = (draft: Draft | undefined) => draft !== undefined && (draft.text !== '' || draft.images.length > 0)

export type DraftSnapshot = Readonly<Record<string, Draft>>

/** 草稿存储失败保留当前操作的反馈；由工作台决定反馈显示的位置。 */
export class DraftStorageError extends Error {
  constructor(message: string, readonly recovery: string) {
    super(message)
    this.name = 'DraftStorageError'
  }
}

/** 草稿的唯一可变所有者；发布给工作台的快照与内部使用同一份不可变引用。 */
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

  /** 编辑立即更新内存；落盘失败时保留页面内的内容并报告存储错误。 */
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

  /** 发送或转移期间继续编辑的内容仍保留。 */
  async clearIfUnchanged(sessionId: string, submitted: Draft): Promise<void> {
    if (this.snapshot?.[sessionId] === submitted) await this.set(sessionId, emptyDraft)
  }

  /** 目标保存成功后才清空源；保存期间源草稿有新编辑时保留新内容。 */
  async transfer(sourceId: string, targetId: string): Promise<void> {
    if (sourceId === targetId) return
    const draft = this.get(sourceId)
    if (hasDraft(draft) && await this.set(targetId, draft)) await this.clearIfUnchanged(sourceId, draft)
  }

  /** 归档或移除已成功；草稿删除成功后才更新快照，失败保留原记录并说明部分完成。 */
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

/** 归档或移除成功后，一次提交对应任务的草稿删除。 */
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

/** 图片字节在发送边界编码；草稿保留 File，避免每次编辑文字复制 Base64。 */
export function imageUpload(file: File): Promise<ImageUpload> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve({ name: file.name, dataUrl: String(reader.result) })
    reader.onerror = () => reject(new Error(`无法读取 ${file.name}：${reader.error?.message}`))
    reader.readAsDataURL(file)
  })
}

export function imageFile({ name, dataUrl }: ImageUpload): File {
  const bytes = Uint8Array.from(atob(dataUrl.split(',')[1]), value => value.charCodeAt(0))
  return new File([bytes], name, { type: dataUrl.slice(5, dataUrl.indexOf(';')) })
}
