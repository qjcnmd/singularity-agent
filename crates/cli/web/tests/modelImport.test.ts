import assert from 'node:assert/strict'
import test from 'node:test'
import { blankModel, mergeDiscoveredModels } from '../src/modelImport.ts'
import { model } from './fixtures.ts'
import type { DiscoveredModel, ReasoningVariant } from '../src/protocol'

const discovered = (overrides: Partial<DiscoveredModel> = {}): DiscoveredModel => ({
  modelId: 'm', displayName: null, maxContextTokens: null, maxOutputTokens: null,
  reasoningVariants: [], defaultVariant: null, thinkingWireFormat: null, ...overrides,
})

const picked = (...ids: string[]) => new Set(ids)
const adopt = (rows: Parameters<typeof mergeDiscoveredModels>[0], candidates: DiscoveredModel[], protocol = 'chat') =>
  mergeDiscoveredModels(rows, candidates, picked(...candidates.map(candidate => candidate.modelId)), protocol)

/** 档位 id 集合：与下拉排序无关，只断言合并规则本身。 */
const variantIds = (row: { reasoningVariants: ReasoningVariant[] }) =>
  row.reasoningVariants.map(variant => variant.id).sort()

test('an unseen model is added from the directory with the form protocol', () => {
  const rows = adopt([], [discovered({
    modelId: 'gpt-x', displayName: 'GPT X', maxContextTokens: 128000, maxOutputTokens: 8192,
    reasoningVariants: [{ id: 'medium', enabled: true, wireEffort: 'medium' }], defaultVariant: 'medium',
  })], 'responses')
  assert.equal(rows.length, 1)
  assert.equal(rows[0].displayName, 'GPT X')
  assert.equal(rows[0].maxContextTokens, 128000)
  assert.equal(rows[0].apiProtocol, 'responses', 'a provider has one protocol in this form')
  assert.deepEqual(rows[0].reasoningVariants, [{ id: 'medium', enabled: true, wireEffort: 'medium' }])
  assert.equal(rows[0].defaultVariant, 'medium', 'a new row may take the directory default')
})

test('unpicked candidates leave the draft untouched', () => {
  const rows = mergeDiscoveredModels([model({ modelId: 'kept' })], [discovered({ modelId: 'ignored' })], picked(), 'chat')
  assert.deepEqual(rows.map(row => row.modelId), ['kept'])
})

/** 配置 off+normal、默认 off，目录只枚举 medium：目录是补充，不是需要替换的目录全集。 */
test('importing keeps the thinking-off variant and the default the user chose', () => {
  const current = model({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'off', enabled: false, wireEffort: null }, { id: 'normal', enabled: true, wireEffort: null }],
    defaultVariant: 'off',
  })
  const candidate = discovered({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'medium', enabled: true, wireEffort: 'medium' }],
    defaultVariant: 'medium',
  })
  const once = adopt([current], [candidate])
  assert.deepEqual(variantIds(once[0]), ['medium', 'normal', 'off'], 'the directory enumeration is added, not substituted')
  assert.deepEqual(
    once[0].reasoningVariants.find(variant => variant.id === 'off'),
    { id: 'off', enabled: false, wireEffort: null },
    'the user-off variant survives a directory that does not enumerate it',
  )
  assert.equal(once[0].defaultVariant, 'off', 'a default that is still available is not replaced')
  assert.deepEqual(adopt(once, [candidate]), once, 'importing again is a no-op')
})

/** 目录也枚举了已有档位时，用户那一行（含 enabled）仍是他自己的声明。 */
test('a directory entry for an existing variant does not rewrite the user row', () => {
  const rows = adopt([model({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'off', enabled: true, wireEffort: null }],
    defaultVariant: 'off',
  })], [discovered({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'off', enabled: false, wireEffort: null }],
    defaultVariant: 'off',
  })])
  assert.deepEqual(rows[0].reasoningVariants, [{ id: 'off', enabled: true, wireEffort: null }])
  assert.equal(rows[0].defaultVariant, 'off')
})

test('user-authored variants are kept alongside the directory enumeration', () => {
  const rows = adopt([model({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'low', enabled: true, wireEffort: 'low' }, { id: 'custom', enabled: true, wireEffort: 'xhigh' }],
    defaultVariant: 'custom',
  })], [discovered({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'low', enabled: true, wireEffort: 'low' }, { id: 'high', enabled: true, wireEffort: 'high' }],
    defaultVariant: 'low',
  })])
  assert.deepEqual(variantIds(rows[0]), ['custom', 'high', 'low'])
  assert.equal(rows[0].defaultVariant, 'custom', 'the directory default only fills a missing or unavailable choice')
})

test('a variant the user renamed keeps its id instead of being duplicated', () => {
  // 同一个 wire effort 换了 id：合并后只有一个档位，用的是用户那行的 id。
  const candidate = discovered({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'medium-legacy', enabled: true, wireEffort: 'medium' }],
    defaultVariant: 'medium-legacy',
  })
  const rows = adopt([model({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'medium', enabled: true, wireEffort: 'medium' }],
    defaultVariant: 'medium',
  })], [candidate])
  assert.deepEqual(variantIds(rows[0]), ['medium'])
  assert.equal(rows[0].defaultVariant, 'medium')
  assert.equal(rows[0].reasoningVariants[0].wireEffort, 'medium')
  assert.deepEqual(adopt(rows, [candidate]), rows, 'importing again is a no-op')
})

/** 用户没设默认而目录默认指向别名 id 时，落到用户保留的那一行。 */
test('a directory default that names an alias resolves to the row the user kept', () => {
  const rows = adopt([model({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'medium', enabled: true, wireEffort: 'medium' }],
    defaultVariant: null,
  })], [discovered({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'medium-legacy', enabled: true, wireEffort: 'medium' }],
    defaultVariant: 'medium-legacy',
  })])
  assert.deepEqual(variantIds(rows[0]), ['medium'])
  assert.equal(rows[0].defaultVariant, 'medium')
})

test('an existing empty draft row is filled from the directory without losing the user protocol', () => {
  const rows = adopt([{ ...blankModel(), modelId: 'reasoner', apiProtocol: 'chat' }], [discovered({
    modelId: 'reasoner',
    maxContextTokens: 64000,
    maxOutputTokens: 4096,
    reasoningVariants: [{ id: 'medium', enabled: true, wireEffort: 'medium' }],
    defaultVariant: 'medium',
  })])
  assert.equal(rows[0].maxContextTokens, 64000)
  assert.equal(rows[0].maxOutputTokens, 4096)
  assert.equal(rows[0].apiProtocol, 'chat')
  assert.deepEqual(variantIds(rows[0]), ['medium'], 'a row without variants takes the directory enumeration')
  assert.equal(rows[0].defaultVariant, 'medium', 'a row without a default takes the directory default')
})

/** 候选没有变体：目录没提供这一项，已有档位与默认都不动。 */
test('a directory without variants leaves the configured variants and default alone', () => {
  const current = model({
    modelId: 'reasoner',
    reasoningVariants: [{ id: 'off', enabled: true, wireEffort: null }, { id: 'custom', enabled: true, wireEffort: null }],
    defaultVariant: 'custom',
  })
  const candidate = discovered({ modelId: 'reasoner' })
  const once = adopt([current], [candidate])
  assert.deepEqual(variantIds(once[0]), ['custom', 'off'])
  assert.equal(once[0].defaultVariant, 'custom')
  assert.deepEqual(adopt(once, [candidate]), once, 'importing again is a no-op')
})

test('importing never overwrites values the user already wrote', () => {
  const rows = adopt([model({
    modelId: 'reasoner',
    displayName: '我的名字',
    maxContextTokens: 32000,
    maxOutputTokens: 2048,
    thinkingWireFormat: 'thinking_type',
    chatOutputTokensField: 'max_completion_tokens',
  })], [discovered({
    modelId: 'reasoner',
    displayName: 'Directory Name',
    maxContextTokens: 200000,
    maxOutputTokens: 16384,
    thinkingWireFormat: 'reasoning_effort',
  })])
  assert.equal(rows[0].displayName, '我的名字')
  assert.equal(rows[0].maxContextTokens, 32000)
  assert.equal(rows[0].maxOutputTokens, 2048)
  assert.equal(rows[0].thinkingWireFormat, 'thinking_type')
  assert.equal(rows[0].chatOutputTokensField, 'max_completion_tokens')
})

test('an existing row keeps its own protocol while new rows take the form protocol', () => {
  const rows = mergeDiscoveredModels(
    [model({ modelId: 'keep', apiProtocol: 'responses' })],
    [discovered({ modelId: 'keep' }), discovered({ modelId: 'added' })],
    picked('keep', 'added'),
    'chat',
  )
  assert.equal(rows[0].apiProtocol, 'responses', 'a per-model protocol override is not reset by importing')
  assert.equal(rows[1].apiProtocol, 'chat', 'a brand-new row follows the form')
})
