/**
 * Shared poll cadence — one timer for the 2s/15s page beat (#129).
 *
 * The market page used to open its own 2s `store.refresh()` interval, and the
 * K-line chart opened a second timer for `kline.history` at the same
 * preference (2s when fastPoll is on, 15s when off). Two timers, one
 * preference, wasted work. This store runs at most one interval while
 * someone is watching:
 *
 *   - snapshot watchers (the market page) trigger `store.refresh()` only
 *     while fastPoll is on — off stays watch-only and keeps the App.vue
 *     15s shell as the snapshot beat.
 *   - tick listeners (the K-line chart, next PR) fire on the same beat,
 *     2s or 15s according to fastPoll, without opening another interval.
 *
 * Leaving the last watcher drops the timer (bandwidth first). Toggling
 * fastPoll rebuilds the interval; it does not start one with nobody
 * watching.
 */
import { defineStore } from 'pinia'
import { watch } from 'vue'
import { usePanelStore } from '@/stores/panel'
import { useSettingsStore } from '@/stores/settings'

const FAST_MS = 2_000
const SLOW_MS = 15_000

export const useCadenceStore = defineStore('cadence', () => {
  const settings = useSettingsStore()

  let snapshotWatchers = 0
  let tickWatchers = 0
  const tickListeners = new Set<() => void>()
  let timer: ReturnType<typeof setInterval> | null = null
  let currentMs = 0

  function intervalMs(): number {
    return settings.fastPoll ? FAST_MS : SLOW_MS
  }

  function snapshotWanted(): boolean {
    return snapshotWatchers > 0 && !!settings.fastPoll
  }

  function tick(): void {
    if (snapshotWanted()) void usePanelStore().refresh()
    for (const fn of tickListeners) fn()
  }

  function ensureTimer(): void {
    const needed = snapshotWanted() || tickWatchers > 0
    const ms = intervalMs()
    if (!needed) {
      if (timer != null) {
        clearInterval(timer)
        timer = null
        currentMs = 0
      }
      return
    }
    if (timer != null && currentMs === ms) return
    if (timer != null) clearInterval(timer)
    currentMs = ms
    // Match the previous useIntervalFn default: first fire after one interval,
    // not immediately (R5).
    timer = setInterval(() => { tick() }, ms)
  }

  function startSnapshotWatch(): void {
    snapshotWatchers += 1
    ensureTimer()
  }

  function stopSnapshotWatch(): void {
    snapshotWatchers = Math.max(0, snapshotWatchers - 1)
    ensureTimer()
  }

  function startTickWatch(fn: () => void): void {
    tickWatchers += 1
    tickListeners.add(fn)
    ensureTimer()
  }

  function stopTickWatch(fn: () => void): void {
    tickWatchers = Math.max(0, tickWatchers - 1)
    tickListeners.delete(fn)
    ensureTimer()
  }

  watch(() => settings.fastPoll, () => { ensureTimer() })

  return { startSnapshotWatch, stopSnapshotWatch, startTickWatch, stopTickWatch }
})
