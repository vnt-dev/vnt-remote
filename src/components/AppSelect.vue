<script setup lang="ts" generic="T extends string | number">
import { computed, nextTick, onBeforeUnmount, ref, useId, watch } from 'vue';

const props = defineProps<{
  modelValue: T;
  options: { value: T; label: string; disabled?: boolean; group?: string }[];
  disabled?: boolean;
  id?: string;
  ariaLabel?: string;
  ariaDescribedby?: string;
}>();
const emit = defineEmits<{
  'update:modelValue': [value: T];
  change: [value: T];
  open: [];
}>();
const uid = useId();
const listId = `select-${uid}`;
const trigger = ref<HTMLButtonElement>();
const menu = ref<HTMLElement>();
const menuTarget = ref<HTMLElement | string>('body');
const opened = ref(false);
const active = ref(-1);
const position = ref({ left: '0px', top: '0px', width: '0px', maxHeight: '260px' });
const selected = computed(() => props.options.find((o) => o.value === props.modelValue));

function close() {
  opened.value = false;
  document.removeEventListener('pointerdown', outside);
  window.removeEventListener('resize', close);
  document.removeEventListener('scroll', onScroll, true);
}
function outside(event: PointerEvent) {
  const target = event.target as Node;
  if (!trigger.value?.contains(target) && !menu.value?.contains(target)) close();
}
function onScroll(event: Event) {
  if (!menu.value?.contains(event.target as Node)) close();
}
function reveal() {
  void nextTick(() =>
    menu.value?.querySelector('[data-active="true"]')?.scrollIntoView({ block: 'nearest' }),
  );
}
async function open() {
  if (props.disabled || opened.value || !trigger.value) return;
  menuTarget.value = trigger.value.closest('dialog') ?? 'body';
  emit('open');
  const rect = trigger.value.getBoundingClientRect();
  const below = window.innerHeight - rect.bottom - 12;
  const above = rect.top - 12;
  const upwards = below < 160 && above > below;
  const height = Math.max(40, Math.min(260, upwards ? above : below));
  position.value = {
    left: `${Math.max(8, Math.min(rect.left, window.innerWidth - rect.width - 8))}px`,
    top: `${upwards ? rect.top - 6 : rect.bottom + 6}px`,
    width: `${Math.min(rect.width, window.innerWidth - 16)}px`,
    maxHeight: `${height}px`,
  };
  opened.value = true;
  active.value = props.options.findIndex((o) => o.value === props.modelValue && !o.disabled);
  if (active.value < 0) active.value = props.options.findIndex((o) => !o.disabled);
  await nextTick();
  if (menu.value) menu.value.style.transform = upwards ? 'translateY(-100%)' : '';
  document.addEventListener('pointerdown', outside);
  window.addEventListener('resize', close);
  document.addEventListener('scroll', onScroll, true);
  reveal();
}
function choose(index: number) {
  const option = props.options[index];
  if (!option || option.disabled) return;
  emit('update:modelValue', option.value);
  if (option.value !== props.modelValue) emit('change', option.value);
  close();
  trigger.value?.focus({ preventScroll: true });
}
function keydown(event: KeyboardEvent) {
  if (event.key === 'Tab') {
    close();
    return;
  }
  if (event.key === 'Escape') {
    event.preventDefault();
    close();
    return;
  }
  if (!['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter', ' '].includes(event.key)) return;
  event.preventDefault();
  if (!opened.value) {
    void open();
    return;
  }
  if (event.key === 'Enter' || event.key === ' ') {
    choose(active.value);
    return;
  }
  const indices = props.options.flatMap((o, i) => (o.disabled ? [] : [i]));
  if (!indices.length) return;
  const current = indices.indexOf(active.value);
  const next =
    event.key === 'Home'
      ? 0
      : event.key === 'End'
        ? indices.length - 1
        : (current + (event.key === 'ArrowUp' ? -1 : 1) + indices.length) % indices.length;
  active.value = indices[next]!;
  reveal();
}
watch(
  () => props.disabled,
  (disabled) => {
    if (disabled) close();
  },
);
onBeforeUnmount(close);
</script>

<template>
  <div class="app-select">
    <button
      :id="id"
      ref="trigger"
      type="button"
      class="select-trigger"
      role="combobox"
      :disabled="disabled"
      :aria-label="ariaLabel"
      :aria-describedby="ariaDescribedby"
      :aria-expanded="opened"
      :aria-controls="listId"
      aria-haspopup="listbox"
      :aria-activedescendant="opened && active >= 0 ? `${listId}-${active}` : undefined"
      :title="selected?.label"
      @click="opened ? close() : open()"
      @keydown="keydown"
      @blur="close"
    >
      <span class="select-label">{{ selected?.label || '请选择' }}</span>
      <svg class="select-chevron" width="14" height="14" viewBox="0 0 16 16" fill="none">
        <path
          d="m4 6 4 4 4-4"
          stroke="currentColor"
          stroke-width="1.5"
          stroke-linecap="round"
          stroke-linejoin="round"
        />
      </svg>
    </button>
    <Teleport :to="menuTarget">
      <div
        v-if="opened"
        :id="listId"
        ref="menu"
        class="select-menu"
        role="listbox"
        :aria-label="ariaLabel"
        :style="position"
        @mousedown.prevent
      >
        <template v-for="(option, index) in options" :key="option.value">
          <div
            v-if="option.group && (index === 0 || option.group !== options[index - 1]?.group)"
            class="select-group"
            role="presentation"
          >
            {{ option.group }}
          </div>
          <div
            :id="`${listId}-${index}`"
            class="select-option"
            role="option"
            :aria-selected="option.value === modelValue"
            :aria-disabled="option.disabled || undefined"
            :data-active="index === active"
            :title="option.label"
            @pointermove="!option.disabled && (active = index)"
            @click="choose(index)"
          >
            <span class="select-label">{{ option.label }}</span>
            <span class="select-check" aria-hidden="true">{{
              option.value === modelValue ? '✓' : ''
            }}</span>
          </div>
        </template>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.select-group {
  padding: 8px 12px 4px;
  font-size: 11px;
  opacity: 0.6;
}
.app-select {
  width: 100%;
  min-width: 0;
  flex: 1;
}
.select-trigger {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  min-width: 0;
  height: 40px;
  padding: 0 12px;
  border: 1px solid var(--border);
  border-radius: 10px;
  background: var(--surface);
  color: var(--text);
  font-size: 13.5px;
  font-weight: 400;
  text-align: left;
  cursor: pointer;
}
.select-trigger:hover:not(:disabled) {
  border-color: var(--text-3);
}
.select-trigger[aria-expanded='true'] {
  border-color: var(--brand);
}
.select-trigger:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.select-label {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.select-chevron,
.select-check {
  flex: none;
}
.select-menu {
  position: fixed;
  z-index: 1000;
  box-sizing: border-box;
  overflow-x: hidden;
  overflow-y: auto;
  padding: 5px;
  border: 1px solid var(--border);
  border-radius: 11px;
  background: var(--surface);
  color: var(--text);
  box-shadow: var(--shadow-pop);
  overscroll-behavior: contain;
}
.select-option {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  padding: 9px 10px;
  border-radius: 7px;
  font-size: 13px;
  font-weight: 400;
  cursor: pointer;
}
.select-option[data-active='true'] {
  background: var(--hover);
}
.select-option[aria-selected='true'] {
  color: var(--brand);
}
.select-option[aria-disabled='true'] {
  opacity: 0.45;
  cursor: not-allowed;
}
.select-check {
  width: 14px;
}
</style>
