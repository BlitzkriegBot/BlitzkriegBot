import { ref } from 'vue'
import { defineStore } from 'pinia'
import {
  api,
  type ExecutionPolicyListDoc,
  type ExecutionPolicySectionDoc,
  type ExecutionPolicyPreviewDoc,
  type ExecutionPolicyHistoryDoc,
} from '@/api/client'

/**
 * Shared cache for execution-policy list / section / preview / history.
 * SettingsRisk keeps the editor buffers and every write (`executionPolicySet`,
 * `riskSetSystemic`, `riskSetExit`, `accountUnfreeze`) on the page (#129).
 */
export const useExecutionPolicyStore = defineStore('execution-policy', () => {
  const policyList = ref<ExecutionPolicyListDoc | null>(null)
  const policySection = ref<ExecutionPolicySectionDoc | null>(null)
  const policyPreview = ref<ExecutionPolicyPreviewDoc | null>(null)
  const policyHistory = ref<ExecutionPolicyHistoryDoc>([])
  const policyErr = ref<string | null>(null)
  const policyBusy = ref(false)

  async function load(accountId: string): Promise<ExecutionPolicySectionDoc | null> {
    policyBusy.value = true
    try {
      const list = await api.executionPolicyList()
      if (list.error) throw new Error(list.error)
      policyList.value = list
      const section = await api.executionPolicyGet(accountId)
      policySection.value = 'error' in (section as object) ? null : section
      try {
        policyPreview.value = await api.executionPolicyPreview(accountId)
      } catch {
        /* preview 缺席不阻塞页面（无已平仓交易/无策略都可能出现） */
      }
      policyHistory.value = await api.executionPolicyHistory(accountId)
      policyErr.value = null
      return policySection.value
    } catch (e) {
      policyErr.value = e instanceof Error ? e.message : String(e)
      return null
    } finally {
      policyBusy.value = false
    }
  }

  return {
    policyList,
    policySection,
    policyPreview,
    policyHistory,
    policyErr,
    policyBusy,
    load,
  }
})
