import { ref } from 'vue'
import { defineStore } from 'pinia'
import { ApiError, api } from '@/api/client'

/**
 * Owns the blueprint compile debounce and last Lua / error so leaving the
 * editor mid-compile does not drop the in-flight work or start a second
 * 350ms timer on remount (#129). Canvas nodes, save, and source-mode
 * load/save stay on BlueprintPage — those are editor session state.
 */
export const useBlueprintDraftStore = defineStore('blueprint-draft', () => {
  const compileError = ref<string | null>(null)
  const luaSource = ref<string | null>(null)
  const compiling = ref(false)
  let compileTimer: ReturnType<typeof setTimeout> | null = null
  let pendingJson: string | null = null

  function cancelPending(): void {
    if (compileTimer) {
      clearTimeout(compileTimer)
      compileTimer = null
    }
  }

  function scheduleCompile(json: string): void {
    pendingJson = json
    cancelPending()
    compileTimer = setTimeout(() => {
      compileTimer = null
      if (pendingJson !== null) void doCompile(pendingJson)
    }, 350)
  }

  async function doCompile(json: string): Promise<void> {
    cancelPending()
    compiling.value = true
    try {
      const doc = await api.blueprintCompile(json)
      if (doc.error) {
        compileError.value = doc.error
        luaSource.value = null
      } else {
        compileError.value = null
        luaSource.value = doc.lua
      }
    } catch (e) {
      compileError.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
      luaSource.value = null
    } finally {
      compiling.value = false
    }
  }

  return {
    compileError,
    luaSource,
    compiling,
    scheduleCompile,
    doCompile,
    cancelPending,
  }
})
