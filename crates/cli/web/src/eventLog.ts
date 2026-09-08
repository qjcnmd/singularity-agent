import type { TurnEventEnvelope } from './protocol'

/** Immutable append-only event suffixes. Earlier React snapshots retain their exact contents. */
export class EventLog {
  readonly length: number
  private readonly first: TurnEventEnvelope | undefined

  constructor(private readonly chunk: readonly TurnEventEnvelope[], private readonly previous?: EventLog) {
    this.length = (previous?.length ?? 0) + chunk.length
    this.first = previous?.first ?? chunk[0]
  }

  append(event: TurnEventEnvelope): EventLog { return new EventLog([event], this) }

  at(index: number): TurnEventEnvelope | undefined {
    if (index < 0) index += this.length
    if (index === 0) return this.first
    if (index < 0 || index >= this.length) return undefined
    let node: EventLog | undefined = this
    while (node) {
      const offset = node.previous?.length ?? 0
      if (index >= offset) return node.chunk[index - offset]
      node = node.previous
    }
    return undefined
  }

  *since(start: number): IterableIterator<TurnEventEnvelope> {
    const suffix: EventLog[] = []
    let node: EventLog | undefined = this
    while (node && node.length > start) { suffix.push(node); node = node.previous }
    for (let index = suffix.length - 1; index >= 0; index--) {
      const node = suffix[index]
      const offset = node.previous?.length ?? 0
      for (let item = Math.max(0, start - offset); item < node.chunk.length; item++) yield node.chunk[item]
    }
  }

  [Symbol.iterator](): IterableIterator<TurnEventEnvelope> { return this.since(0) }
  toJSON(): TurnEventEnvelope[] { return [...this] }
}

export type EventSequence = readonly TurnEventEnvelope[] | EventLog
export function appendEvent(events: EventSequence, event: TurnEventEnvelope): EventLog {
  return (events instanceof EventLog ? events : new EventLog(events)).append(event)
}
export function eventsSince(events: EventSequence, start: number): Iterable<TurnEventEnvelope> {
  return events instanceof EventLog ? events.since(start) : events.slice(start)
}
export function isEventPrefix(previous: EventSequence, events: EventSequence): boolean {
  return previous.length <= events.length && previous.at(0) === events.at(0)
    && previous.at(-1) === events.at(previous.length - 1)
}
