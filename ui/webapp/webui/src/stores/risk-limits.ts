import { ref } from 'vue'
import { defineStore } from 'pinia'
import { api, type RiskLimitsDoc } from '@/api/client'

/**
 * Shared cache for the live risk-limits document (`risk.limits`).
 * SettingsRiskLimits is the only reader today; moving the fetch here keeps
 * the page as the edit-buffer owner and leaves write paths (`risk.setSystemic`
 * / `risk.setExit`) on the settings pages unchanged (#129).
 */
export const useRiskLimitsStore = defineStore('risk-limits', () => {
  const riskDoc = ref<RiskLimitsDoc | null>(null)
  const riskErr = ref<string | null>(null)
  const riskBusy = ref(false)

  async function load(): Promise<RiskLimitsDoc | null> {
    riskBusy.value = true
    try {
      const doc = await api.riskLimits()
      riskDoc.value = doc
      riskErr.value = doc.error ?? null
      return doc
    } catch (e) {
      riskErr.value = e instanceof Error ? e.message : String(e)
      return null
    } finally {
      riskBusy.value = false
    }
  }

  return { riskDoc, riskErr, riskBusy, load }
})
