<script setup lang="ts">
import { computed, nextTick, ref } from 'vue';
import { captureTargets, targetKey, toggleTarget } from '../services/captureTargets';
import { api, type AppConfig, type MonitorInfo, type ScreenRegion } from '../services/api';

const props = defineProps<{
  config: AppConfig;
  monitors: MonitorInfo[];
  disabled: boolean;
  save: (patch: Partial<AppConfig>) => Promise<void>;
  mode?: 'selection' | 'management';
}>();
const emit = defineEmits<{ refresh: [] }>();
const busy = ref(false);
const error = ref('');
const manage = ref<HTMLDialogElement>();
const naming = ref<HTMLDialogElement>();
const draft = ref<ScreenRegion | null>(null);
const editing = ref(false);
const regions = computed(() => props.config.screenRegions ?? []);
const selected = computed(() => captureTargets(props.config, props.monitors));
const selectedKeys = computed(() => new Set(selected.value.map(targetKey)));
function monitor(region: ScreenRegion) {
  return props.monitors.find((m) => m.name === region.monitorName);
}
function valid(region: ScreenRegion) {
  const m = monitor(region);
  return (
    m &&
    m.width === region.screenWidth &&
    m.height === region.screenHeight &&
    m.orientation === region.orientation
  );
}
function display(region: ScreenRegion) {
  const m = monitor(region);
  return m ? (m.primary ? '主屏' : `显示器 ${m.index + 1}`) : '显示器离线';
}
const options = computed(() => [
  ...props.monitors.map((m) => ({
    value: `monitor:${m.name}`,
    target: { kind: 'monitor' as const, name: m.name },
    label: `${m.primary ? '主屏' : `显示器 ${m.index + 1}`} · ${m.width} × ${m.height}`,
    group: '显示器',
  })),
  ...regions.value.map((r) => ({
    value: `region:${r.id}`,
    target: { kind: 'region' as const, id: r.id },
    label: `${r.name} · ${display(r)} · ${r.width} × ${r.height}${valid(r) ? '' : ' · 不可用，请重新框选'}`,
    group: '屏幕区域',
    disabled: !valid(r),
  })),
]);
async function write(patch: Partial<AppConfig>) {
  busy.value = true;
  error.value = '';
  try {
    await props.save(patch);
    return true;
  } catch (e) {
    error.value = String(e);
    return false;
  } finally {
    busy.value = false;
  }
}
async function choose(value: string) {
  const option = options.value.find((option) => option.value === value);
  if (option)
    await write({
      selectedTargets: toggleTarget(selected.value, option.target),
    });
}
async function pick(region?: ScreenRegion) {
  if (busy.value) return;
  manage.value?.close();
  error.value = '';
  busy.value = true;
  try {
    const selected = await api.pickScreenRegion();
    emit('refresh');
    if (selected) {
      editing.value = !!region;
      let n = 1;
      while (regions.value.some((r) => r.name === `区域 ${n}`)) n++;
      draft.value = {
        ...selected,
        id: region?.id ?? crypto.randomUUID(),
        name: region?.name ?? `区域 ${n}`,
      };
      await nextTick();
      naming.value?.showModal();
    } else if (region) manage.value?.showModal();
  } catch (e) {
    error.value = String(e);
    if (region) manage.value?.showModal();
  } finally {
    busy.value = false;
  }
}
async function rename(region: ScreenRegion) {
  manage.value?.close();
  editing.value = true;
  draft.value = { ...region };
  error.value = '';
  await nextTick();
  naming.value?.showModal();
}
function cancelName() {
  naming.value?.close();
  draft.value = null;
  if (editing.value) manage.value?.showModal();
}
async function saveDraft() {
  if (!draft.value || busy.value) return;
  const region = { ...draft.value, name: draft.value.name.trim() };
  if (!region.name || [...region.name].length > 64) {
    error.value = '请输入 1–64 字的区域名称';
    return;
  }
  const next = editing.value
    ? regions.value.map((r) => (r.id === region.id ? region : r))
    : [...regions.value, region];
  if (
    await write({
      screenRegions: next,
      ...(!editing.value
        ? {
            selectedTargets: [...selected.value, { kind: 'region' as const, id: region.id }],
          }
        : {}),
    })
  ) {
    naming.value?.close();
    draft.value = null;
    if (editing.value) manage.value?.showModal();
  }
}
async function remove(region: ScreenRegion) {
  await write({
    screenRegions: regions.value.filter((r) => r.id !== region.id),
    selectedTargets: selected.value.filter((t) => t.kind !== 'region' || t.id !== region.id),
  });
}
</script>

<template>
  <div class="field grow capture-field">
    <span v-if="mode !== 'management'" class="lb">采集屏幕</span>
    <div class="capture-controls">
      <details v-if="mode !== 'management'" class="capture-select" @toggle="emit('refresh')">
        <summary>已选择 {{ selected.length }} 个屏幕或区域</summary>
        <div class="capture-options">
          <fieldset v-for="group in ['显示器', '屏幕区域']" :key="group">
            <legend>{{ group }}</legend>
            <label v-for="option in options.filter((o) => o.group === group)" :key="option.value">
              <input
                type="checkbox"
                :checked="selectedKeys.has(option.value)"
                :disabled="
                  disabled ||
                  busy ||
                  Boolean(
                    'disabled' in option && option.disabled && !selectedKeys.has(option.value),
                  )
                "
                @change="choose(option.value)"
              />
              {{ option.label }}
            </label>
          </fieldset>
        </div>
      </details>
      <button
        v-if="mode === 'management'"
        type="button"
        class="region-button"
        :disabled="disabled || busy || regions.length >= 100"
        @click="pick()"
      >
        添加区域
      </button>
      <button
        v-if="mode === 'management'"
        type="button"
        class="region-button"
        :disabled="disabled || busy || !regions.length"
        @click="
          error = '';
          emit('refresh');
          manage?.showModal();
        "
      >
        管理区域
      </button>
    </div>
    <span v-if="mode !== 'management' && !selected.length" class="region-error"
      >请选择采集屏幕或区域</span
    >
    <span
      v-if="
        mode !== 'management' &&
        config.selectedTargets.some(
          (t) => t.kind === 'region' && regions.some((r) => r.id === t.id && !valid(r)),
        )
      "
      class="region-error"
      >所选区域不可用，请重新框选或选择其他目标</span
    >
    <span v-if="error && !draft" class="region-error" role="alert">{{ error }}</span>
  </div>
  <Teleport v-if="mode === 'management'" to="body">
    <dialog
      ref="manage"
      class="region-dialog"
      aria-label="管理屏幕区域"
      @cancel="busy && $event.preventDefault()"
    >
      <div class="region-heading">
        <h3>管理屏幕区域</h3>
        <button :disabled="busy" class="region-button" @click="manage?.close()">关闭</button>
      </div>
      <p class="region-hint">区域修改与删除对下次连接生效，已连接会话保持原区域。</p>
      <div v-for="region in regions" :key="region.id" class="region-row">
        <div class="region-description">
          <strong>{{ region.name }}</strong
          ><span
            >{{ display(region) }} · {{ region.width }} × {{ region.height }} · ({{ region.x }},
            {{ region.y }})</span
          ><span v-if="!valid(region)" class="region-error"
            >显示器不可用或显示模式已变化，请重新框选</span
          >
        </div>
        <button class="region-button" :disabled="busy" @click="rename(region)">重命名</button>
        <button class="region-button" :disabled="busy" @click="pick(region)">重新框选</button>
        <button class="region-button" :disabled="busy" @click="remove(region)">删除</button>
      </div>
      <p v-if="!regions.length" class="region-hint">暂无屏幕区域</p>
      <p v-if="error" class="region-error" role="alert">{{ error }}</p>
    </dialog>
    <dialog
      ref="naming"
      class="region-dialog naming-dialog"
      aria-label="保存屏幕区域"
      @cancel.prevent="!busy && cancelName()"
    >
      <form v-if="draft" @submit.prevent="saveDraft">
        <h3>{{ editing ? '编辑屏幕区域' : '添加屏幕区域' }}</h3>
        <p class="region-hint">
          {{ display(draft) }} · {{ draft.width }} × {{ draft.height }} 像素
        </p>
        <label class="region-name"
          >区域名称<input v-model="draft.name" autofocus maxlength="64" required :disabled="busy"
        /></label>
        <p v-if="error" class="region-error" role="alert">{{ error }}</p>
        <div class="region-actions">
          <button type="button" class="region-button" :disabled="busy" @click="cancelName">
            取消</button
          ><button type="submit" class="region-button primary" :disabled="busy">
            {{ busy ? '保存中…' : '保存' }}
          </button>
        </div>
      </form>
    </dialog>
  </Teleport>
</template>

<style scoped>
.capture-select {
  position: relative;
  min-width: 220px;
}
.capture-select summary {
  cursor: pointer;
  padding: 8px 12px;
  border: 1px solid var(--border);
  border-radius: 6px;
}
.capture-options {
  position: absolute;
  z-index: 20;
  top: 100%;
  min-width: 360px;
  max-height: 360px;
  overflow: auto;
  padding: 12px;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  box-shadow: 0 8px 24px #0003;
}
.capture-options fieldset {
  border: 0;
  margin: 0;
  padding: 4px 0;
}
.capture-options legend {
  opacity: 0.7;
  font-size: 12px;
}
.capture-options label {
  display: flex;
  gap: 8px;
  align-items: center;
  padding: 6px 0;
  white-space: nowrap;
}
.capture-field {
  min-width: 0;
}
.capture-controls {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 7px;
}
.capture-controls :deep(.app-select) {
  min-width: 160px;
}
.region-button {
  white-space: nowrap;
  padding: 9px 11px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--surface);
  color: var(--text);
  cursor: pointer;
  font: inherit;
  font-size: 12px;
}
.region-button:disabled {
  opacity: 0.45;
  cursor: default;
}
.region-button:hover:enabled {
  border-color: var(--accent, #69a5ff);
}
.region-hint {
  display: block;
  margin: 7px 0 0;
  font-size: 11px;
  color: var(--muted);
  line-height: 1.5;
}
.region-error {
  display: block;
  color: #e57868;
  font-size: 12px;
  margin-top: 5px;
}
.region-dialog {
  width: min(720px, calc(100vw - 80px));
  max-height: 75vh;
  padding: 24px;
  border: 1px solid var(--border);
  border-radius: 16px;
  background: var(--surface);
  color: var(--text);
  box-shadow: 0 16px 70px #0005;
}
.region-dialog::backdrop {
  background: #0007;
}
.region-heading,
.region-row,
.region-actions {
  display: flex;
  align-items: center;
  gap: 10px;
}
.region-heading {
  justify-content: space-between;
}
h3 {
  margin: 0;
  font-size: 17px;
}
.region-row {
  padding: 16px 0;
  border-bottom: 1px solid var(--border);
}
.region-description {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
  overflow-wrap: anywhere;
}
.region-description > span {
  font-size: 12px;
}
.naming-dialog {
  width: 380px;
}
.region-name {
  display: flex;
  flex-direction: column;
  gap: 8px;
  font-size: 12px;
  margin-top: 20px;
}
.region-name input {
  padding: 11px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--surface);
  color: var(--text);
  font: inherit;
  font-size: 14px;
}
.region-actions {
  justify-content: flex-end;
  margin-top: 24px;
}
.primary {
  background: var(--accent, #417ae8);
  color: white;
}
</style>
