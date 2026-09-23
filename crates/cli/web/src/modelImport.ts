import type { DiscoveredModel, ModelConfigurationInput, ReasoningVariant } from './protocol'
import { sortReasoningVariants } from './modelChoices'

/**
 * 采纳提供方发现结果的纯规则：草稿行的新建与合并。
 *
 * 设置编辑器把选中集合交给这里，导入规则由这一处维护。
 */

/** 新建草稿的初始值；提供方与协议由调用方补齐。 */
export function blankModel(): ModelConfigurationInput {
  return {
    modelId: '',
    displayName: null,
    apiProtocol: 'chat',
    maxContextTokens: null,
    maxOutputTokens: null,
    reasoningVariants: [],
    defaultVariant: null,
    thinkingWireFormat: null,
    chatOutputTokensField: null,
  }
}

/**
 * 同一档位的身份：id 相同，或候选声明了 wire effort 而已有变体声明同一个 ——
 * 用户改过 id 时仍是同一档位，目录枚举不会因此多出一行。
 */
function sameVariant(id: string, wireEffort: string | null, variant: ReasoningVariant): boolean {
  return variant.id === id || (wireEffort !== null && variant.wireEffort === wireEffort)
}

/**
 * 目录枚举只补充已有声明：已有变体（含用户自己关闭的 off 与自定义档位）原样
 * 保留，目录里有而配置里没有的档位追加到末尾。目录没枚举某个档位不等于用户删掉
 * 它，显式删除仍由用户在编辑器里的操作完成。
 */
function mergeVariants(current: ReasoningVariant[], imported: ReasoningVariant[]): ReasoningVariant[] {
  const merged = [...current]
  for (const variant of imported) {
    if (!merged.some(existing => sameVariant(variant.id, variant.wireEffort, existing))) merged.push(variant)
  }
  return merged
}

/**
 * 把档位 id 对应到合并结果里的实际 id：候选默认先按同一身份规则解析，用户用了
 * 别名的档位不会被目录的 id 顶成空引用。返回 null 表示合并结果里没有这一档位。
 */
function resolveVariantId(
  defaultVariant: string | null,
  declared: ReasoningVariant[],
  merged: ReasoningVariant[],
): string | null {
  if (defaultVariant === null) return null
  const wireEffort = declared.find(variant => variant.id === defaultVariant)?.wireEffort ?? null
  return merged.find(variant => sameVariant(defaultVariant, wireEffort, variant))?.id ?? null
}

/**
 * 把选中的发现结果并入草稿行。
 *
 * 合并优先级：已有行的用户内容与仍有效的选择保留，候选只补空缺，不把「目录没
 * 返回」当成删除。新模型按目录值新建并采用表单当前协议；已有行不覆盖用户已经
 * 写过的名称、容量、思考词形与输出上限字段，否则一次「获取可用模型」会悄悄
 * 改掉用户按实际端点核对过的值。变体枚举直接复用模型选择下拉的 `sortReasoningVariants`，
 * 二者不可能互相矛盾；默认档位只在原值已不在合并结果里时才由候选补上。
 */
export function mergeDiscoveredModels(
  rows: ModelConfigurationInput[],
  candidates: DiscoveredModel[],
  picked: ReadonlySet<string>,
  protocol: string,
): ModelConfigurationInput[] {
  const next = [...rows]
  for (const candidate of candidates) {
    if (!picked.has(candidate.modelId)) continue
    const at = next.findIndex(row => row.modelId.trim() === candidate.modelId)
    if (at < 0) {
      next.push({ ...blankModel(), ...candidate, apiProtocol: protocol })
      continue
    }
    const current = next[at]
    const reasoningVariants = sortReasoningVariants(mergeVariants(current.reasoningVariants, candidate.reasoningVariants))
    next[at] = {
      ...current,
      displayName: current.displayName || candidate.displayName,
      maxContextTokens: current.maxContextTokens ?? candidate.maxContextTokens,
      maxOutputTokens: current.maxOutputTokens ?? candidate.maxOutputTokens,
      reasoningVariants,
      // 用户选定的默认档位只要还在合并结果里就保持；否则才用候选默认补齐，
      // 候选也没给出可用档位时保留原值，导入不负责删除或改写用户选择。
      defaultVariant: resolveVariantId(current.defaultVariant, current.reasoningVariants, reasoningVariants)
        ?? resolveVariantId(candidate.defaultVariant, candidate.reasoningVariants, reasoningVariants)
        ?? current.defaultVariant,
    }
  }
  return next
}
