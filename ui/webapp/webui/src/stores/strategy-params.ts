import { ref } from 'vue'
import { defineStore } from 'pinia'
import { api, type StrategyParamsReadoutDoc } from '@/api/client'

/**
 * Shared caches for strategy tunables and the arb-config tab gate.
 * Edit drafts stay here so expanding the same strategy on a remount does not
 * flash empty inputs; write path (`strategy.params.set`) is unchanged (#129).
 */
export const useStrategyParamsStore = defineStore('strategy-params', () => {
  const paramsCache = ref<Record<string, StrategyParamsReadoutDoc>>({})
  const paramsDraft = ref<Record<string, string>>({})
  const paramsBusy = ref<string | null>(null)
  const arbConfigured = ref(false)
  const arbLegAccounts = ref<string[]>([])

  function draftOf(name: string): Record<string, string> {
    const prefix = `${name}::`
    const out: Record<string, string> = {}
    for (const k of Object.keys(paramsDraft.value)) {
      if (k.startsWith(prefix)) out[k.slice(prefix.length)] = paramsDraft.value[k]
    }
    return out
  }

  async function loadParams(name: string): Promise<void> {
    try {
      const doc = await api.strategyParams(name)
      paramsCache.value = { ...paramsCache.value, [name]: doc }
      const fresh: Record<string, string> = {}
      for (const t of doc.tunables) fresh[`${name}::${t.name}`] = t.value
      paramsDraft.value = { ...paramsDraft.value, ...fresh }
    } catch {
      paramsCache.value = {
        ...paramsCache.value,
        [name]: {
          name,
          found: false,
          tunables: [],
          evolvable: false,
          paramsAttached: false,
          note: '读取失败',
        },
      }
    }
  }

  async function saveParams(name: string): Promise<void> {
    const draft = draftOf(name)
    const cache = paramsCache.value[name]
    if (!cache) return
    const patch: Record<string, string> = {}
    for (const t of cache.tunables) {
      const v = (draft[t.name] ?? '').trim()
      if (v && v !== t.value) patch[t.name] = v
    }
    if (!Object.keys(patch).length) return
    paramsBusy.value = name
    try {
      await api.strategyParamsSet({ name, params: patch })
      await loadParams(name)
    } catch (e) {
      console.error(`参数保存失败 ${name}:`, e instanceof Error ? e.message : String(e))
    } finally {
      paramsBusy.value = null
    }
  }

  async function loadArbConfig(): Promise<void> {
    try {
      const doc = await api.arbConfigGet()
      if (doc.error) return
      arbConfigured.value = !!doc.configured
      arbLegAccounts.value = [doc.legAAccount, doc.legBAccount].filter(
        (x): x is string => typeof x === 'string' && !!x,
      )
    } catch {
      /* 网络失败：单所 tab 照常，套利 tab 不渲染 */
    }
  }

  return {
    paramsCache,
    paramsDraft,
    paramsBusy,
    arbConfigured,
    arbLegAccounts,
    draftOf,
    loadParams,
    saveParams,
    loadArbConfig,
  }
})
