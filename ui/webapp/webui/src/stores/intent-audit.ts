/**
 * Intent-audit store — the single write-up path for the arbitration tail (#129).
 *
 * The Decisions page used to poll `api.intentAuditTail` every 4s on its own.
 * That copy lived only on the page, so a remount started from a blank list
 * and any second consumer would have opened another timer. The store owns
 * the fetch, caches the last tail, and runs at most one 4s timer while
 * someone is watching. Leaving the page drops the timer (bandwidth first).
 */
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type IntentAuditRecordView } from '@/api/client'

const TAIL_LIMIT = 200
const POLL_MS = 4_000

export const useIntentAuditStore = defineStore('intent-audit', () => {
  const records = ref<IntentAuditRecordView[]>([])
  const error = ref('')
  const loaded = ref(false)

  let watchers = 0
  let timer: ReturnType<typeof setInterval> | null = null
  let inFlight: Promise<void> | null = null

  async function fetchTail(): Promise<void> {
    if (inFlight) return inFlight
    inFlight = (async () => {
      try {
        const doc = await api.intentAuditTail({ limit: TAIL_LIMIT })
        if (doc.error) {
          error.value = doc.error
          return
        }
        error.value = ''
        loaded.value = true
        records.value = doc.records ?? []
      } catch (e) {
        error.value = String((e as Error)?.message ?? e)
      } finally {
        inFlight = null
      }
    })()
    return inFlight
  }

  function startWatching(): void {
    watchers += 1
    if (watchers !== 1) return
    void fetchTail()
    timer = setInterval(() => { void fetchTail() }, POLL_MS)
  }

  function stopWatching(): void {
    watchers = Math.max(0, watchers - 1)
    if (watchers > 0 || timer == null) return
    clearInterval(timer)
    timer = null
  }

  return { records, error, loaded, startWatching, stopWatching }
})
