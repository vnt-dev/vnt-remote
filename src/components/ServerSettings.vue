<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue';
import { api, type LinkConfig, type ProfileStatus } from '../services/api';
import RoomLabel from './RoomLabel.vue';
import {
  connectionIdLabel,
  linkPhaseLabel,
  profileConnectionError,
  profileNameError,
} from '../services/linkProfiles';

const props = defineProps<{ statuses: ProfileStatus[] }>();
const emit = defineEmits<{ saved: [] }>();
const configs = ref<LinkConfig[]>([]);
const computerName = ref('');
const draft = ref<LinkConfig | null>(null);
const editorDialog = ref<HTMLDialogElement>();
watch(draft, async (value) => {
  await nextTick();
  if (value && !editorDialog.value?.open) editorDialog.value?.showModal();
  else if (!value) editorDialog.value?.close();
});
const connectedCount = computed(() => props.statuses.filter((s) => s.phase === 'connected').length);
function closeEditor() {
  if (!saving.value) draft.value = null;
}

const addresses = ref('');
const busyIds = ref<string[]>([]);
const errors = ref<Record<string, string>>({});
const loadError = ref('');
const loading = ref(true);
const saving = computed(() => !!draft.value && busyIds.value.includes(draft.value.id));
const nameError = computed(() => (draft.value ? profileNameError(draft.value, configs.value) : ''));
const connectionError = computed(() =>
  draft.value
    ? profileConnectionError({
        ...draft.value,
        servers: addresses.value
          .split(/\r?\n|,/)
          .map((s) => s.trim())
          .filter(Boolean),
      })
    : '',
);
async function load() {
  try {
    const [config, name] = await Promise.all([api.getConfig(), api.computerName()]);
    configs.value = config.servers ?? [];
    computerName.value = name;
    loadError.value = '';
  } catch (e) {
    loadError.value = String(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);
function edit(config?: LinkConfig) {
  draft.value = config
    ? {
        ...config,
        deviceName: config.deviceName || computerName.value,
        servers: [...config.servers],
      }
    : {
        id: crypto.randomUUID(),
        name: '',
        deviceName: computerName.value,
        enabled: false,
        mode: 'manual',
        subscription: '',
        servers: ['vnt2.rustvnt.com:29872'],
        networkCode: '',
        connectionId: String(
          100_000_000 + (crypto.getRandomValues(new Uint32Array(1))[0] % 900_000_000),
        ),
        encryptionPassword: '',
      };
  addresses.value = draft.value.servers.join('\n');
}
function status(id: string) {
  return props.statuses.find((s) => s.configId === id);
}
function updateConnectionId(event: Event) {
  const input = event.target as HTMLInputElement;
  input.value = input.value.replace(/[^0-9]/g, '').slice(0, 9);
  if (draft.value) draft.value.connectionId = input.value;
}
async function apply(config: LinkConfig, enabled = config.enabled) {
  if (busyIds.value.includes(config.id)) return;
  const error = profileNameError(config, configs.value) || profileConnectionError(config);
  if (error) {
    errors.value[config.id] = error;
    return;
  }
  busyIds.value.push(config.id);
  errors.value[config.id] = '';
  try {
    await api.serverLinkApply({ ...config, name: config.name.trim(), enabled });
    if (draft.value?.id === config.id) draft.value = null;
    await load();
    emit('saved');
  } catch (e) {
    errors.value[config.id] = String(e);
  } finally {
    busyIds.value = busyIds.value.filter((id) => id !== config.id);
  }
}
function save() {
  if (!draft.value) return;
  void apply({
    ...draft.value,
    servers: addresses.value
      .split(/\r?\n|,/)
      .map((s) => s.trim())
      .filter(Boolean),
  });
}
async function remove(config: LinkConfig) {
  if (busyIds.value.includes(config.id)) return;
  busyIds.value.push(config.id);
  errors.value[config.id] = '';
  try {
    await api.serverLinkRemove(config.id);
    if (draft.value?.id === config.id) draft.value = null;
    await load();
    emit('saved');
  } catch (e) {
    errors.value[config.id] = String(e);
  } finally {
    busyIds.value = busyIds.value.filter((id) => id !== config.id);
  }
}
</script>

<template>
  <div class="server-page">
    <div class="page-head">
      <div class="grow">
        <div class="heading-title">
          <h1>服务端</h1>
          <span v-if="configs.length" class="summary"
            >{{ configs.length }} 个配置<span class="summary-dot">·</span> >{{
              connectedCount
            }}
            个已连接</span
          >
        </div>
        <p>连接服务端，同步同组设备。</p>
      </div>
      <button
        type="button"
        class="btn btn-primary"
        :disabled="loading || !!loadError || !!draft"
        @click="edit()"
      >
        <svg class="ic sm"><use href="#i-plus" /></svg>添加配置
      </button>
    </div>
    <p v-if="loadError" class="error-text" role="alert">
      {{ loadError }} <button type="button" class="btn btn-ghost btn-sm" @click="load">重试</button>
    </p>
    <p v-if="loading" class="sub" role="status">正在加载…</p>
    <div v-else-if="!configs.length && !loadError" class="dev-grid">
      <button type="button" class="dev-add" :disabled="!!draft" @click="edit()">
        <svg class="ic" style="width: 20px; height: 20px"><use href="#i-plus" /></svg>
        添加配置
        <small>订阅或手动配置</small>
      </button>
    </div>
    <div class="profile-grid">
      <article
        v-for="config in configs"
        :key="config.id"
        class="profile-card"
        :class="{ connected: status(config.id)?.phase === 'connected' }"
      >
        <div class="profile-header">
          <div class="profile-icon">
            <svg class="ic"><use href="#i-wifi" /></svg>
          </div>
          <div class="profile-title">
            <h2 :title="config.name">{{ config.name }}</h2>
            <span>{{ config.mode === 'subscription' ? '订阅' : '手动配置' }}</span>
          </div>
          <span class="phase" :class="status(config.id)?.phase"
            ><span class="dot"></span
            >{{ linkPhaseLabel(status(config.id)?.phase ?? 'stopped') }}</span
          >
        </div>
        <div class="profile-info">
          <div>
            <RoomLabel class="info-label" /><span
              class="info-value num"
              :title="status(config.id)?.networkCode || config.networkCode"
              >{{ status(config.id)?.networkCode || config.networkCode || '—' }}</span
            >
          </div>
          <div>
            <span class="info-label">在线设备</span
            ><span class="info-value"
              >{{ status(config.id)?.devices.length ?? 0 }}<small>台</small></span
            >
          </div>
        </div>
        <p
          v-if="errors[config.id] || status(config.id)?.error"
          class="error-text profile-error"
          role="alert"
        >
          {{ errors[config.id] || status(config.id)?.error }}
        </p>
        <div class="profile-footer">
          <span class="local-address num">{{
            connectionIdLabel(status(config.id)?.localConnectionId ?? '') || '尚未连接'
          }}</span>
          <div class="profile-actions">
            <button
              type="button"
              class="icon-button"
              aria-label="编辑配置"
              title="编辑"
              :disabled="busyIds.includes(config.id) || !!draft"
              @click="edit(config)"
            >
              <svg class="ic sm"><use href="#i-edit" /></svg>
            </button>
            <button
              type="button"
              class="icon-button delete-button"
              aria-label="删除配置"
              title="删除"
              :disabled="busyIds.includes(config.id)"
              @click="remove(config)"
            >
              <svg class="ic sm"><use href="#i-trash" /></svg>
            </button>
            <button
              type="button"
              class="btn btn-sm toggle-button"
              :class="config.enabled ? 'btn-secondary' : 'btn-primary'"
              :disabled="busyIds.includes(config.id)"
              @click="apply(config, !config.enabled)"
            >
              {{ busyIds.includes(config.id) ? '处理中…' : config.enabled ? '停止' : '启动' }}
            </button>
          </div>
        </div>
      </article>
    </div>
  </div>
  <Teleport to="body">
    <dialog
      ref="editorDialog"
      class="profile-dialog"
      aria-labelledby="profile-editor-title"
      @cancel.prevent="closeEditor"
      @click="$event.target === editorDialog && closeEditor()"
    >
      <form v-if="draft" @submit.prevent="save">
        <div class="editor-header">
          <h2 id="profile-editor-title">
            {{ configs.some((c) => c.id === draft?.id) ? '编辑配置' : '添加配置' }}
          </h2>
          <button
            type="button"
            class="icon-button"
            aria-label="关闭"
            :disabled="saving"
            @click="closeEditor"
          >
            <svg class="ic sm"><use href="#i-x" /></svg>
          </button>
        </div>
        <div class="editor-body">
          <label class="field"
            ><span class="lb">配置名称</span
            ><span class="inp"
              ><input
                v-model="draft.name"
                placeholder="例如 办公室"
                :disabled="saving"
                required
                maxlength="64"
                :aria-invalid="!!draft.name && !!nameError"
                aria-describedby="profile-name-error"
                autofocus /></span
          ></label>
          <p v-if="draft.name && nameError" id="profile-name-error" class="error-text" role="alert">
            {{ nameError }}
          </p>
          <div class="seg mode-switch" role="group" aria-label="服务端配置模式">
            <button
              v-for="mode in ['subscription', 'manual']"
              :key="mode"
              type="button"
              :class="{ on: draft.mode === mode }"
              :disabled="saving"
              @click="draft.mode = mode"
            >
              {{ mode === 'subscription' ? '订阅' : '手动配置' }}
            </button>
          </div>
          <label v-if="draft.mode === 'subscription'" class="field"
            ><span class="lb">订阅地址</span
            ><span class="inp"
              ><input
                v-model="draft.subscription"
                required
                type="text"
                placeholder="vnt2://join/2/…"
                autocomplete="off"
                :disabled="saving" /></span
          ></label>
          <template v-else>
            <label class="field"
              ><span class="lb">服务器地址</span
              ><textarea
                v-model="addresses"
                required
                rows="3"
                placeholder="每行一个地址，例如 vnt2.rustvnt.com:29872"
                :disabled="saving"
              ></textarea>
            </label>
            <div class="editor-grid">
              <label class="field"
                ><RoomLabel class="lb" /><span class="inp"
                  ><input
                    v-model="draft.networkCode"
                    required
                    placeholder="房间编号"
                    autocomplete="off"
                    :disabled="saving" /></span
              ></label>
              <label class="field"
                ><span class="lb">连接 ID</span
                ><span class="inp"
                  ><input
                    :value="draft.connectionId"
                    @input="updateConnectionId"
                    required
                    placeholder="1～9 位纯数字"
                    inputmode="numeric"
                    pattern="[0-9]{1,9}"
                    minlength="1"
                    maxlength="9"
                    autocomplete="off"
                    :disabled="saving" /></span
              ></label>
              <label class="field"
                ><span class="lb">设备名称 <span class="optional">可选</span></span
                ><span class="inp"
                  ><input
                    v-model="draft.deviceName"
                    placeholder="默认使用电脑名"
                    :disabled="saving"
                    maxlength="64" /></span
              ></label>
              <label class="field"
                ><span class="lb">加密密码 <span class="optional">可选</span></span
                ><span class="inp"
                  ><input
                    v-model="draft.encryptionPassword"
                    type="password"
                    placeholder="同组设备保持一致"
                    autocomplete="new-password"
                    :disabled="saving" /></span
              ></label>
            </div>
          </template>
          <p v-if="errors[draft.id]" class="error-text" role="alert">{{ errors[draft.id] }}</p>
        </div>
        <div class="editor-footer">
          <button
            type="button"
            class="btn btn-secondary btn-sm"
            :disabled="saving"
            @click="closeEditor"
          >
            取消</button
          ><button
            type="submit"
            class="btn btn-primary btn-sm"
            :disabled="saving || !!nameError || !!connectionError"
          >
            {{ saving ? '保存中…' : '保存配置' }}
          </button>
        </div>
      </form>
    </dialog>
  </Teleport>
</template>

<style scoped>
.required {
  color: var(--danger);
}
.heading-title {
  display: flex;
  align-items: baseline;
  gap: 16px;
}
.summary,
.local-address {
  font-size: 12px;
  color: var(--text-3);
}
.summary-dot {
  padding: 0 8px;
}
.profile-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 18px;
}
.profile-card {
  min-width: 0;
  border: 1px solid var(--border);
  border-radius: 16px;
  background: var(--surface);
  overflow: hidden;
  transition:
    border-color 0.18s,
    box-shadow 0.18s;
}
.profile-card:hover {
  border-color: var(--brand-border);
  box-shadow: 0 4px 20px rgb(0 0 0 / 4%);
}
.profile-header {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 22px 22px 18px;
}
.profile-icon {
  display: grid;
  place-items: center;
  width: 42px;
  height: 42px;
  border-radius: 12px;
  background: var(--surface-2, var(--brand-tint));
  color: var(--text-3);
  flex-shrink: 0;
}
.connected .profile-icon {
  background: var(--brand-tint);
  color: var(--brand);
}
.profile-title {
  flex: 1;
  min-width: 0;
}
.profile-title h2 {
  margin: 0 0 5px;
  font-size: 15px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.profile-title > span,
.info-label {
  font-size: 12px;
  color: var(--text-3);
}
.phase {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 11px;
  white-space: nowrap;
  color: var(--text-3);
}
.phase.connected {
  color: var(--brand);
}
.phase.connected .dot {
  background: var(--brand);
}
.phase.error {
  color: var(--danger, #d33);
}
.phase.error .dot {
  background: currentColor;
}
.profile-info {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 100px;
  gap: 20px;
  padding: 0 22px 22px;
}
.profile-info > div {
  display: flex;
  flex-direction: column;
  gap: 7px;
  min-width: 0;
}
.info-value {
  font-size: 16px;
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.info-value small {
  font-size: 11px;
  font-weight: 400;
  color: var(--text-3);
  margin-left: 5px;
}
.profile-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 18px 12px 22px;
  border-top: 1px solid var(--border);
}
.profile-actions {
  display: flex;
  align-items: center;
  gap: 5px;
}
.toggle-button {
  min-width: 64px;
  margin-left: 7px;
}
.icon-button {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 30px;
  height: 30px;
  padding: 0;
  border: 0;
  border-radius: 7px;
  color: var(--text-3);
  background: transparent;
  cursor: pointer;
}
.icon-button:hover {
  color: var(--text);
  background: var(--brand-tint);
}
.delete-button:hover {
  color: var(--danger, #d33);
}
.icon-button:disabled {
  opacity: 0.4;
  cursor: default;
}
.profile-error {
  padding: 0 22px 14px;
  margin: 0;
  overflow-wrap: anywhere;
}
.mode-switch {
  align-self: flex-start;
}
.optional {
  font-size: 11px;
  color: var(--text-3);
  font-weight: 400;
  margin-left: 5px;
}
textarea {
  width: 100%;
  resize: vertical;
  min-height: 84px;
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 10px 12px;
  background: var(--surface);
  color: var(--text);
  font: inherit;
}
textarea:focus {
  outline: 2px solid var(--brand-border);
  outline-offset: 1px;
}
@media (max-width: 900px) {
  .profile-grid {
    grid-template-columns: minmax(0, 1fr);
  }
}
@media (max-width: 480px) {
  .summary {
    display: none;
  }
}
</style>
