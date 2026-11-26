<script setup lang="ts">
/**
 * Unified Select primitive — pixel-matched in height, radius, border, and focus
 * ring to Input, Button, and SegmentedControl (sm = 32px / default = 36px / lg = 40px).
 */
import { ChevronDown } from 'lucide-vue-next'
import { cn } from '@/lib/utils'

export interface SelectOption {
  value: string
  label: string
  disabled?: boolean
}

const props = withDefaults(
  defineProps<{
    modelValue: string
    options?: SelectOption[]
    size?: 'sm' | 'default' | 'lg'
    disabled?: boolean
    ariaLabel?: string
    class?: string
  }>(),
  { size: 'default', disabled: false },
)
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const SIZE_CLASSES: Record<'sm' | 'default' | 'lg', string> = {
  sm: 'h-8 pl-2.5 pr-7 text-[12.5px]',
  default: 'h-9 pl-3 pr-8 text-[13px]',
  lg: 'h-10 pl-3.5 pr-9 text-[13.5px]',
}
</script>

<template>
  <div :class="cn('relative inline-flex w-full items-center', props.class)">
    <select
      :value="props.modelValue"
      :disabled="props.disabled"
      :aria-label="props.ariaLabel"
      :class="cn(
        'w-full appearance-none rounded-md border border-line bg-panel-2 text-fg',
        'shadow-[inset_0_1px_2px_oklch(0_0_0_/_0.22),0_1px_0_var(--sheen)]',
        'transition-[border-color,box-shadow] duration-150 outline-none',
        'hover:border-line-strong focus:border-primary/60 focus:shadow-[0_0_0_3px_oklch(0.76_0.17_62_/_0.18)]',
        'disabled:cursor-not-allowed disabled:opacity-45',
        SIZE_CLASSES[props.size],
      )"
      @change="emit('update:modelValue', ($event.target as HTMLSelectElement).value)"
    >
      <template v-if="props.options">
        <option
          v-for="opt in props.options"
          :key="opt.value"
          :value="opt.value"
          :disabled="opt.disabled"
        >
          {{ opt.label }}
        </option>
      </template>
      <slot v-else />
    </select>
    <ChevronDown
      class="pointer-events-none absolute right-2.5 size-3.5 text-muted-fg"
      aria-hidden="true"
    />
  </div>
</template>
