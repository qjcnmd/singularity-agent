import { WorkbenchStore, type WorkbenchState } from '../src/store'
import type { WorkbenchTransport, StreamListener, StatusListener } from '../src/connection'
import type { RpcMethod, RpcParams, RpcResult, StreamEnvelope } from '../src/protocol'
import type { SessionReadResult } from '../src/protocol.generated'
import { storageKey } from '../src/viewPersistence'
import { bootstrap, readyFrame, session } from './fixtures'

export class MemoryStorage implements Storage {
  private values = new Map<string, string>()
  get length() { return this.values.size }
  getItem(key: string) { return this.values.get(key) ?? null }
  setItem(key: string, value: string) { this.values.set(key, String(value)) }
  removeItem(key: string) { this.values.delete(key) }
  clear() { this.values.clear() }
  key(index: number) { return [...this.values.keys()][index] ?? null }
}

export class FakeTransport implements WorkbenchTransport {
  readonly calls: Array<{ method: RpcMethod; params: unknown }> = []
  private handlers = new Map<RpcMethod, (params: never) => unknown>()
  reconnects = 0

  constructor(private onFrame: StreamListener, private onStatus: StatusListener) {}
  respond<M extends RpcMethod>(method: M, handler: (params: RpcParams<M>) => RpcResult<M> | Promise<RpcResult<M>>): void {
    this.handlers.set(method, handler)
  }
  async rpc<M extends RpcMethod>(method: M, params: RpcParams<M>): Promise<RpcResult<M>> {
    this.calls.push({ method, params })
    const handler = this.handlers.get(method)
    if (!handler) throw new Error(`Unexpected RPC ${method}`)
    // Only respond() populates this map, retaining each method's parameter/result relation.
    return await handler(params as never) as RpcResult<M>
  }
  emit(frame: StreamEnvelope) { this.onFrame(frame) }
  status(status: Parameters<StatusListener>[0]) { this.onStatus(status) }
  start() { this.status('ready'); this.emit(readyFrame()) }
  stop() {}
  reconnect() { this.reconnects++ }
}

export const tick = () => new Promise<void>(resolve => setImmediate(resolve))
export const deferred = <T>() => Promise.withResolvers<T>()

export function waitFor(store: WorkbenchStore, predicate: (state: WorkbenchState) => boolean): Promise<void> {
  if (predicate(store.getSnapshot())) return Promise.resolve()
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { unsubscribe(); reject(new Error('Store did not reach the expected state')) }, 2000)
    const unsubscribe = store.subscribe(() => {
      if (predicate(store.getSnapshot())) { clearTimeout(timer); unsubscribe(); resolve() }
    })
  })
}

export async function harness(options: {
  bootstrap?: ReturnType<typeof bootstrap>; session?: SessionReadResult; selectedSessionId?: string | null;
} = {}) {
  const initial = options.bootstrap ?? bootstrap()
  const selectedSessionId = options.selectedSessionId === undefined ? 's' : options.selectedSessionId
  localStorage.setItem(storageKey, JSON.stringify({ version: 1, selectedWorkspaceId: 'w', selectedSessionId }))
  let transport!: FakeTransport
  const store = new WorkbenchStore({ createTransport: (onFrame, onStatus) => (transport = new FakeTransport(onFrame, onStatus)) })
  transport.respond('workbench.bootstrap', () => initial)
  transport.respond('session.read', params => options.session ?? session({ summary: { ...session().summary, threadId: params.sessionId } }))
  store.start()
  await waitFor(store, state => state.bootstrap !== null && state.connection === 'ready' && state.sessionLoad.status === 'idle'
    && (state.selectedSessionId === null || state.session?.summary.threadId === state.selectedSessionId))
  await tick()
  return { store, transport, bootstrap: initial }
}
