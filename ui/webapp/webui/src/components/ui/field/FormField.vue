<script setup lang="ts">
/**
 * Standardized form-field wrapper — enforces consistent label typography and
 * vertical rhythm so adjacent Input, Select, SegmentedControl, and Button
 * controls share an exact bottom baseline.
 */
import { cn } from '@/lib/utils'

const props = defineProps<{
  label?: string
  hint?: string
  class?: string
}>()
</script>

<template>
  <label :class="cn('flex flex-col gap-1.5 min-w-0', props.class)">
    <span v-if="props.label" class="label-micro truncate">{{ props.label }}</span>
    <slot />
    <span v-if="props.hint" class="text-[11px] text-faint-fg">{{ props.hint }}</span>
  </label>
</template>
