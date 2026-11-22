/**
 * Backtest job store — the single write-up path for backtest/pull job status (#129).
 *
 * The Backtest page used to run its own 1s `setInterval` over
 * `api.backtestStatus` and keep both the pull jobs and the backtest job in
 * component-local refs. That copy died with the page (a remount lost the
 * running job) and every extra consumer would have opened another timer.
 *
 * The store owns:
 *   - the two job lists (`pullDocs`, `btDoc`) and their poll cadence,
 *   - the "all pulls settled → refresh the dataset list once" edge,
 *   - the completion hook that collects the result envelope.
 *
 * Cadence rule (#129 design §3): a timer runs ONLY while at least one job is
 * still running, and it stops the moment the last one settles. Leaving the page
 * does not stop a job — the kernel keeps working and the operator may navigate
 * away and come back — so visibility is reference-counted on the page, while
 * the timer itself is driven by job state.
 */
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { api, type BacktestJobStatusDoc, type BacktestResultDoc } from '@/api/client'

const POLL_MS = 1_000

export const useBacktestJobStore = defineStore('backtest-jobs', () => {
  /** Chain-pull jobs started from this panel session. */
  const pullDocs = ref<BacktestJobStatusDoc[]>([])
  /** The backtest run job (one at a time — the page starts it). */
  const btDoc = ref<BacktestJobStatusDoc | null>(null)
  /** Result of the last finished backtest, fetched once on completion. */
  const lastResult = ref<BacktestResultDoc | null>(null)
  /** Id of the last backtest started here (export target). */
  const lastBtId = ref<string | null>(null)
  /** All pulls settled — the page refreshes the dataset list exactly once. */
  const pullsSettled = ref(false)
  /** Result-collection failure (the job finished but the envelope did not). */
  const resultError = ref<string | null>(null)

  const pullBusy = computed(() => pullDocs.value.some((d) => d.state === 'running'))
  const btRunning = computed(() => btDoc.value?.state === 'running')
  /** Any job still in flight — the only condition that keeps the timer alive. */
  const anyRunning = computed(() => pullBusy.value || btRunning.value)

  let timer: ReturnType<typeof setInterval> | null = null
  /** Page-level visibility refcount: the timer only runs while someone watches. */
  let watchers = 0
  /** Set by the page: called once when all pulls settle (dataset list refresh). */
  let onPullsSettled: (() => void | Promise<void>) | null = null

  function stopTimer(): void {
    if (timer !== null) {
      clearInterval(timer)
      timer = null
    }
  }

  function ensureTimer(): void {
    if (watchers === 0 || !anyRunning.value) {
      stopTimer()
      return
    }
    if (timer === null) timer = setInterval(() => { void pollOnce() }, POLL_MS)
  }

  /** Poll every running job once. Never invents a settled state on error. */
  async function pollOnce(): Promise<void> {
    for (const d of pullDocs.value) {
      if (d.state !== 'running') continue
      try {
        Object.assign(d, await api.backtestStatus(d.jobId))
      } catch {
        /* transient network blip — the next tick retries */
      }
    }
    if (pullDocs.value.length > 0 && !pullBusy.value && !pullsSettled.value) {
      pullsSettled.value = true
      await onPullsSettled?.()
    }

    const b = btDoc.value
    if (b?.state === 'running') {
      try {
        const nd = await api.backtestStatus(b.jobId)
        btDoc.value = nd
        if (nd.state === 'done') await collectResult(nd.jobId)
      } catch {
        /* keep polling */
      }
    }
    // Nothing left in flight — drop the timer (bandwidth first).
    if (!anyRunning.value) stopTimer()
  }

  async function collectResult(id: string): Promise<void> {
    // A finished job whose envelope failed to load is NOT "no result" — say so
    // and let the operator retry the export; never leave the page silent (R7.1 L4).
    resultError.value = null
    try {
      lastResult.value = await api.backtestResult(id)
    } catch (e) {
      resultError.value = e instanceof Error ? e.message : String(e)
    }
  }

  /** Register the pull job ids the kernel handed back and start watching them. */
  function startPulls(jobIds: string[]): void {
    pullsSettled.value = false
    pullDocs.value = jobIds.map((jobId) => ({
      jobId,
      kind: 'pull',
      state: 'running',
      phase: 'queued',
      detail: '已提交，等待内核受理…',
      progress: 0,
      startedAtMs: Date.now(),
      finishedAtMs: null,
      error: null,
    }))
    ensureTimer()
  }

  /** Register a backtest run job. */
  function startBacktest(jobId: string): void {
    lastBtId.value = jobId
    lastResult.value = null
    btDoc.value = {
      jobId,
      kind: 'backtest',
      state: 'running',
      phase: 'replay',
      detail: '已提交，引擎正在虚拟时钟里重放数据集…',
      progress: 0,
      startedAtMs: Date.now(),
      finishedAtMs: null,
      error: null,
    }
    ensureTimer()
  }

  /** Page mounted: count a watcher; resume the timer if a job is still running. */
  function attach(): void {
    watchers += 1
    ensureTimer()
  }

  /** Page unmounted: drop the watcher and the timer (job state is kept). */
  function detach(): void {
    watchers = Math.max(0, watchers - 1)
    if (watchers === 0) stopTimer()
  }

  /** The page registers its dataset-list refresh here (one call per settle). */
  function setOnPullsSettled(fn: (() => void | Promise<void>) | null): void {
    onPullsSettled = fn
  }

  return {
    pullDocs, btDoc, lastResult, lastBtId, pullsSettled, resultError,
    pullBusy, btRunning, anyRunning,
    startPulls, startBacktest, attach, detach, setOnPullsSettled,
    pollOnce,
  }
})
