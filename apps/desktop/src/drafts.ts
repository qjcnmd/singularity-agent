import type { ImageUpload } from './protocol'

export interface Draft { text: string; images: File[]; skills?: Record<string, string> }
export const emptyDraft: Draft = { text: '', images: [] }
export const hasDraft = (draft: Draft | undefined) => draft !== undefined && (draft.text !== '' || draft.images.length > 0)

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

export async function persistDraft(id: string, draft: Draft): Promise<void> {
  const transaction = (await openDatabase()).transaction('drafts', 'readwrite')
  const store = transaction.objectStore('drafts')
  if (hasDraft(draft)) store.put(draft, id)
  else store.delete(id)
  await complete(transaction)
}

/** 归档或移除成功后，一次提交对应任务的草稿删除。 */
export async function removeDrafts(ids: string[]): Promise<void> {
  const transaction = (await openDatabase()).transaction('drafts', 'readwrite')
  const store = transaction.objectStore('drafts')
  for (const id of ids) store.delete(id)
  await complete(transaction)
}

export async function loadDrafts(): Promise<Record<string, Draft>> {
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
  // 只迁移旧版文字草稿，成功提交后才删除旧键；已有新记录优先。
  const prefix = 'singularity.app.view.v1:draft:'
  for (let index = localStorage.length - 1; index >= 0; index--) {
    const key = localStorage.key(index)
    if (!key?.startsWith(prefix)) continue
    const id = key.slice(prefix.length)
    const text = localStorage.getItem(key) ?? ''
    if (result[id] === undefined && text !== '') {
      result[id] = { text, images: [] }
      await persistDraft(id, result[id])
    }
    localStorage.removeItem(key)
  }
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
