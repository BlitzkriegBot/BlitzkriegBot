<script setup lang="ts">
/**
 * Segmented control — height-locked to Button, Input, and Select
 * (sm = h-8 / 32px, default = h-9 / 36px). Keyboard-navigable with a sliding
 * gold indicator.
 */
import { computed } from 'vue'
import { cn } from '@/lib/utils'

export interface Segment {
  id: string
  label: string
  badge?: string | number
}

const props = withDefaults(
  defineProps<{
    modelValue: string
    segments: Segment[]
    size?: 'sm' | 'default'
    class?: string
    /** When true, inner tab buttons do not emit. */
    disabled?: boolean
  }>(),
  { size: 'default', disabled: false },
)
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const activeIndex = computed(() => Math.max(0, props.segments.findIndex((s) => s.id === props.modelValue)))
const count = computed(() => Math.max(1, props.segments.length))
</script>

<template>
  <div
    :class="cn(
      'relative inline-grid items-center gap-0.5 rounded-md border border-line bg-panel-2 p-0.5 backdrop-blur-md min-w-0',
      props.size === 'sm' ? 'h-8' : 'h-9',
      props.disabled && 'pointer-events-none opacity-60',
      props.class,
    )"
    :style="{ gridTemplateColumns: `repeat(${count}, minmax(0, 1fr))` }"
    role="tablist"
  >
    <!-- sliding indicator -->
    <span
      class="pointer-events-none absolute top-0.5 bottom-0.5 rounded-[5px] btn-gold transition-[left] duration-300 ease-[var(--ease-spring)]"
      :style="{
        left: `calc(${activeIndex} * (100% - 4px) / ${count} + 2px)`,
        width: `calc((100% - 4px) / ${count})`,
      }"
    />
    <button
      v-for="s in props.segments"
      :key="s.id"
      type="button"
      role="tab"
      :aria-selected="s.id === props.modelValue"
      :disabled="props.disabled"
      :class="cn(
        'relative z-10 inline-flex h-full items-center justify-center gap-1.5 rounded-[5px] font-semibold whitespace-nowrap',
        'transition-colors duration-200 focus-visible:outline-none',
        props.size === 'sm' ? 'px-3 text-[12px]' : 'px-3.5 text-[12.5px]',
        s.id === props.modelValue ? 'text-primary-ink' : 'text-muted-fg hover:text-fg',
      )"
      @click="!props.disabled && emit('update:modelValue', s.id)"
    >
      {{ s.label }}
      <span
        v-if="s.badge !== undefined"
        :class="cn(
          'rounded-full px-1.5 py-px text-[10px] font-bold tabular-nums',
          s.id === props.modelValue ? 'bg-primary-ink/18 text-primary-ink' : 'bg-line text-faint-fg',
        )"
      >{{ s.badge }}</span>
    </button>
  </div>
</template>
