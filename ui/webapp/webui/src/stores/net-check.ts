/**
 * Network-diagnosis store (#129 leftover).
 *
 * SettingsGateway used to keep `netCheck` / `probeNetCheck` results and a
 * 1.2s `setTimeout` poll (while `probing`) on the page. Leaving the tab
 * dropped the timer and a remount started from a blank card. The store
 * caches the last document and runs at most one poll loop while someone
 * is watching and the probe is still in flight. Leaving the last watcher
 * drops the timer (bandwidth first); the cached document stays.
 */
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type NetCheckDoc } from '@/api/client'

const POLL_MS = 1_200

export const useNetCheckStore = defineStore('net-check', () => {
  const netDoc = ref<NetCheckDoc | null>(null)
  const netErr = ref<string | null>(null)
  const netBusy = ref(false)

  let watchers = 0
  let timer: ReturnType<typeof setTimeout> | null = null

  function clearTimer(): void {
    if (timer != null) {
      clearTimeout(timer)
      timer = null
    }
  }

  function schedulePoll(): void {
    clearTimer()
    if (watchers <= 0) return
    if (netDoc.value?.probing) {
      timer = setTimeout(() => { void loadNet() }, POLL_MS)
    }
  }

  async function loadNet(): Promise<void> {
    try {
      netDoc.value = await api.netCheck()
      netErr.value = null
    } catch (e) {
      netErr.value = e instanceof Error ? e.message : String(e)
    }
    schedulePoll()
  }

  async function probeNet(): Promise<void> {
    netBusy.value = true
    try {
      netDoc.value = await api.probeNetCheck()
      netErr.value = null
    } catch (e) {
      netErr.value = e instanceof Error ? e.message : String(e)
    } finally {
      netBusy.value = false
      schedulePoll()
    }
  }

  function startWatching(): void {
    watchers += 1
    if (watchers === 1) void loadNet()
    else schedulePoll()
  }

  function stopWatching(): void {
    watchers = Math.max(0, watchers - 1)
    if (watchers === 0) clearTimer()
  }

  return { netDoc, netErr, netBusy, loadNet, probeNet, startWatching, stopWatching }
})
