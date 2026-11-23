/**
 * Version / update in-flight poll (#129 leftover).
 *
 * SettingsVersion used two recursive 1.5s `setTimeout` loops that called
 * `store.refresh()` while an update-check or a stage download was in flight.
 * The timers and the busy flags lived only on the page, so leaving the
 * tab aborted the visible progress (the kernel work kept going) and a
 * remount started from a blank busy state. The store owns the command,
 * the deadline, and at most one timer per action, and keeps running
 * until the snapshot moves or the deadline hits — even if the page is
 * not mounted (a 45s / 150s cap, bandwidth first).
 */
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'
import { usePanelStore } from '@/stores/panel'

const CHECK_POLL_MS = 1_500
const CHECK_DEADLINE_MS = 45_000
const STAGE_POLL_MS = 1_500
const STAGE_DEADLINE_MS = 150_000

export const useVersionUpdateStore = defineStore('version-update', () => {
  const panel = usePanelStore()

  const versionErr = ref<string | null>(null)
  const checkBusy = ref(false)
  const stageBusy = ref(false)
  const stageErr = ref<string | null>(null)

  let checkTimer: ReturnType<typeof setTimeout> | null = null
  let stageTimer: ReturnType<typeof setTimeout> | null = null
  let checkDeadlineAt = 0
  let stageDeadlineAt = 0
  let checkBeforeMs: number | null | undefined = undefined

  function clearCheckTimer(): void {
    if (checkTimer != null) {
      clearTimeout(checkTimer)
      checkTimer = null
    }
  }

  function clearStageTimer(): void {
    if (stageTimer != null) {
      clearTimeout(stageTimer)
      stageTimer = null
    }
  }

  async function pollCheck(): Promise<void> {
    await panel.refresh()
    const now = panel.snapshot?.systemVersion
    const moved = now && now.lastCheckMs !== checkBeforeMs && now.lastCheckMs !== null
    if (moved || Date.now() > checkDeadlineAt) {
      checkBusy.value = false
      clearCheckTimer()
      return
    }
    checkTimer = setTimeout(() => { void pollCheck() }, CHECK_POLL_MS)
  }

  async function checkNow(): Promise<void> {
    const version = panel.snapshot?.systemVersion
    if (!version || checkBusy.value) return
    const before = version.lastCheckMs
    try {
      const res = await api.updateCheck()
      if (res.error) {
        versionErr.value = res.error
        return
      }
    } catch (e) {
      versionErr.value = e instanceof Error ? e.message : String(e)
      return
    }
    checkBeforeMs = before
    checkBusy.value = true
    checkDeadlineAt = Date.now() + CHECK_DEADLINE_MS
    void pollCheck()
  }

  async function configure(body: { autoUpdate?: boolean; checkEnabled?: boolean }): Promise<void> {
    try {
      const res = await api.updateConfigure(body)
      if (res.ok === false && res.error) {
        versionErr.value = res.error
      }
    } catch (e) {
      versionErr.value = e instanceof Error ? e.message : String(e)
    } finally {
      await panel.refresh()
    }
  }

  async function toggleAutoUpdate(on: boolean): Promise<void> {
    await configure({ autoUpdate: on })
  }

  async function toggleUpdateCheck(on: boolean): Promise<void> {
    await configure({ checkEnabled: on })
  }

  async function pollStage(): Promise<void> {
    await panel.refresh()
    const now = panel.snapshot?.stageState
    const settled = now && now.phase !== 'downloading'
    if (settled || Date.now() > stageDeadlineAt) {
      stageBusy.value = false
      clearStageTimer()
      return
    }
    stageTimer = setTimeout(() => { void pollStage() }, STAGE_POLL_MS)
  }

  async function stageNow(): Promise<void> {
    if (stageBusy.value) return
    try {
      const res = await api.updateStage()
      if (res.error) {
        stageErr.value = res.error
        return
      }
    } catch (e) {
      stageErr.value = e instanceof Error ? e.message : String(e)
      return
    }
    stageBusy.value = true
    stageDeadlineAt = Date.now() + STAGE_DEADLINE_MS
    void pollStage()
  }

  return {
    versionErr, checkBusy, stageBusy, stageErr,
    checkNow, stageNow, configure, toggleAutoUpdate, toggleUpdateCheck,
  }
})
