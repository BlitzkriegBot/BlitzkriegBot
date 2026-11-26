<script setup lang="ts">
import { cn } from '@/lib/utils'

// Card section header: optional identity icon + micro label + right-side slot
// for controls. issue 393 (②): the title row reads 「icon + 文字」 — the identity
// icon leads, the label follows — and because every card in the panel composes
// THIS component, the order is fixed in one place instead of per page.
defineProps<{ label?: string; class?: string }>()
</script>

<template>
  <header :class="cn('flex flex-wrap items-center justify-between gap-x-3 gap-y-2 mb-3.5 min-w-0', $props.class)">
    <div class="flex items-center gap-2 shrink-0 min-w-0">
      <slot name="title" />
      <h2 v-if="label" class="label-micro whitespace-nowrap">{{ label }}</h2>
    </div>
    <div class="flex items-center gap-2 min-w-0 max-w-full flex-wrap">
      <slot name="action" />
    </div>
  </header>
</template>
