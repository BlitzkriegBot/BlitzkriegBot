<script setup lang="ts">
/**
 * Unified text/number input primitive — height-locked to Button, Select, and
 * SegmentedControl (sm = 32px / h-8, default = 36px / h-9, lg = 40px / h-10).
 */
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    modelValue: string
    type?: string
    placeholder?: string
    autocomplete?: string
    disabled?: boolean
    size?: 'sm' | 'default' | 'lg'
    class?: string
  }>(),
  { type: 'text', size: 'default', disabled: false },
)
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const SIZE_CLASSES: Record<'sm' | 'default' | 'lg', string> = {
  sm: 'h-8 px-2.5 text-[12.5px]',
  default: 'h-9 px-3 text-[13px]',
  lg: 'h-10 px-3.5 text-[13.5px]',
}
</script>

<template>
  <input
    :type="props.type"
    :value="props.modelValue"
    :placeholder="props.placeholder"
    :autocomplete="props.autocomplete"
    :disabled="props.disabled"
    :class="cn(
      'w-full rounded-md border border-line bg-panel-2 text-fg placeholder:text-faint-fg',
      'shadow-[inset_0_1px_2px_oklch(0_0_0_/_0.22),0_1px_0_var(--sheen)]',
      'transition-[border-color,box-shadow] duration-150 outline-none',
      'hover:border-line-strong focus:border-primary/60 focus:shadow-[0_0_0_3px_oklch(0.76_0.17_62_/_0.18)]',
      'disabled:cursor-not-allowed disabled:opacity-45',
      SIZE_CLASSES[props.size],
      props.class,
    )"
    @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
  >
</template>
