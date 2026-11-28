<script setup lang="ts">
import { computed } from 'vue'
import type { RiskExitView } from '@/api/client'
import Input from '@/components/ui/input/Input.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import { EXIT_ADVANCED_GROUPS, EXIT_FIELD_DEFS, type ExitFieldDef } from './exit-config-fields'

const props = defineProps<{
  exit: RiskExitView
  bufs: Record<string, string>
}>()

const emit = defineEmits<{
  (e: 'update:field', key: string, value: string): void
}>()

const INLINE_KEYS = new Set<keyof RiskExitView>([
  'forceExitSec',
  'trailingEnabled',
  'strategyExitMakerFirst',
])

const groupedFields = computed(() =>
  EXIT_ADVANCED_GROUPS.map((g) => ({
    ...g,
    fields: EXIT_FIELD_DEFS.filter((f) => f.group === g.id && !INLINE_KEYS.has(f.key)),
  })).filter((g) => g.fields.length > 0),
)

function isSupported(f: ExitFieldDef): boolean {
  return props.exit[f.key] !== undefined
}

function boolVal(f: ExitFieldDef): boolean {
  return (props.bufs[f.key] ?? 'false') === 'true'
}

function textVal(f: ExitFieldDef): string {
  return props.bufs[f.key] ?? ''
}

function onBoolChange(f: ExitFieldDef, v: boolean): void {
  emit('update:field', f.key, String(v))
}

function onTextChange(f: ExitFieldDef, v: string | number): void {
  emit('update:field', f.key, String(v))
}

function inputType(f: ExitFieldDef): 'number' | 'text' {
  return f.kind === 'nonneg-int' || f.kind === 'pos-int' || f.kind === 'signed-int'
    ? 'number'
    : 'text'
}

function inputMin(f: ExitFieldDef): number | undefined {
  if (f.kind === 'pos-int') return 1
  if (f.kind === 'nonneg-int') return 0
  return undefined
}
</script>

<template>
  <div class="space-y-3 border-t border-line pt-2" data-exit-config-advanced>
    <div
      v-for="(group, gi) in groupedFields"
      :key="group.id"
      :class="gi === 0 ? '' : 'border-t border-line pt-2'"
    >
      <div class="label-micro">{{ group.title }}</div>
      <div class="mt-1 space-y-1">
        <div
          v-for="(f, fi) in group.fields"
          :key="f.key"
          class="flex items-center justify-between gap-3 py-1 text-[12px]"
          :class="fi === 0 ? '' : 'border-t border-line'"
        >
          <span class="text-faint-fg">
            {{ f.label }}
            <span v-if="f.hint" class="ml-1 text-[10.5px] text-faint-fg">{{ f.hint }}</span>
          </span>

          <span v-if="f.kind === 'bool'" class="flex items-center gap-2">
            <Switch
              :model-value="boolVal(f)"
              :disabled="!isSupported(f)"
              :aria-label="f.label"
              @update:model-value="(v: boolean) => onBoolChange(f, v)"
            />
            <span class="w-10 text-right text-[10.5px] text-faint-fg">
              {{ isSupported(f) ? (boolVal(f) ? '开' : '关') : '不支持' }}
            </span>
          </span>

          <span v-else class="flex items-center gap-1.5">
            <Input
              :model-value="textVal(f)"
              class="num h-7 w-24 text-right text-[12px]"
              :type="inputType(f)"
              :min="inputMin(f)"
              :step="inputType(f) === 'number' ? 1 : 'any'"
              :disabled="!isSupported(f)"
              :placeholder="isSupported(f) ? String(exit[f.key]) : '不支持'"
              :aria-label="f.label"
              @update:model-value="(v: string | number) => onTextChange(f, v)"
            />
            <span class="w-10 text-right text-[10.5px] text-faint-fg">{{ f.unit ?? '' }}</span>
          </span>
        </div>
      </div>
    </div>
  </div>
</template>
