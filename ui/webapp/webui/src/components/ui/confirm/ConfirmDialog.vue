<script setup lang="ts">
/**
 * 非阻塞确认弹层 —— 替代 `window.confirm`，headless 友好。
 *
 * 不阻塞 JS 线程：调用方经 `v-model:open` 控制显隐，点「确认」后
 * emit `confirm`，实际 IO 由调用方执行，完成后自行 `open = false` 收起。
 * 忙态由调用方经 `busy` 注入，期间两个按钮均禁用。
 */
import { TriangleAlert } from 'lucide-vue-next'
import Button from '@/components/ui/button/Button.vue'

const props = defineProps<{
  /** v-model:是否打开 */
  open: boolean
  /** 弹层标题 */
  title: string
  /** 确认说明文案（可选） */
  message?: string
  /** 确认按钮文案 */
  confirmLabel?: string
  /** 取消按钮文案 */
  cancelLabel?: string
  /** 调用方执行中，两个按钮均禁用 */
  busy?: boolean
}>()

const emit = defineEmits<{
  'update:open': [v: boolean]
  /** 用户点击确认按钮，执行权交回调用方 */
  confirm: []
}>()

function cancel(): void {
  emit('update:open', false)
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      @click.self="cancel"
    >
      <div class="w-full max-w-md rounded-lg border border-line bg-panel-solid shadow-lg">
        <div class="flex items-center gap-2 border-b border-line px-4 py-3">
          <TriangleAlert class="size-4 text-down" />
          <h3 class="text-[13.5px] font-semibold">{{ title }}</h3>
        </div>

        <div v-if="message" class="px-4 py-3">
          <p class="text-[12.5px] leading-snug text-muted-fg">{{ message }}</p>
        </div>

        <div class="flex justify-end gap-2 border-t border-line px-4 py-3">
          <Button variant="ghost" size="sm" :disabled="busy" @click="cancel">
            {{ cancelLabel || '取消' }}
          </Button>
          <Button
            variant="danger-solid"
            size="sm"
            :disabled="busy"
            @click="emit('confirm')"
          >
            {{ busy ? '执行中…' : confirmLabel || '确认' }}
          </Button>
        </div>
      </div>
    </div>
  </Teleport>
</template>
