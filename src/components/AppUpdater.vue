<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { updater } from '../services/updater';
import { edition, loadEdition } from '../services/edition';
import { api } from '../services/api';

onMounted(() => {
  void loadEdition().catch(() => undefined);
});

defineProps<{ compact?: boolean }>();
const { state, percent, busy, checkForUpdates, downloadUpdate, installUpdate } = updater;
const status = computed(() => {
  switch (state.phase) {
    case 'checking':
      return '正在检查更新…';
    case 'available':
      return `发现新版本 v${state.version}`;
    case 'latest':
      return '当前已是最新版本';
    case 'downloading':
      return `正在下载 v${state.version}`;
    case 'ready':
      return `v${state.version} 已下载，等待安装`;
    case 'installing':
      return '正在启动安装程序…';
    default:
      return '检查并安装新版本';
  }
});
const visible = computed(() => ['downloading', 'ready', 'installing'].includes(state.phase));
function bytes(value: number) {
  return `${(value / 1024 / 1024).toFixed(1)} MB`;
}
</script>

<template>
  <div v-if="edition === 'portable' && !compact" class="app-updater">
    <div class="st">免安装版更新</div>
    <p class="sd">下载新版免安装 EXE，退出程序后替换。设备列表和界面偏好会保留。</p>
    <button @click="api.openReleasePage()">前往发布页下载</button>
  </div>
  <div
    v-else-if="edition === 'installed' && (!compact || visible)"
    class="app-updater"
    aria-live="polite"
  >
    <div class="st">{{ status }}</div>
    <template v-if="visible">
      <progress :value="percent" max="100" aria-label="更新下载进度"></progress>
      <div class="sd num">
        {{ bytes(state.downloaded)
        }}<template v-if="state.total"> / {{ bytes(state.total) }}</template> ·
        {{ percent === undefined ? '下载中' : `${percent}%` }}
      </div>
    </template>
    <template v-if="!compact">
      <p v-if="state.notes" class="release-notes sd">{{ state.notes }}</p>
      <p class="sd">安装时将关闭应用和远程控制窗口，并升级本机被控服务；可能需要管理员授权。</p>
      <p v-if="state.error" class="error-text" role="alert">{{ state.error }}</p>
      <div class="update-actions">
        <button :disabled="busy || state.phase === 'ready'" @click="checkForUpdates">
          检查更新
        </button>
        <button v-if="state.phase === 'available'" @click="downloadUpdate">下载更新</button>
        <button v-if="state.phase === 'ready'" @click="installUpdate">安装更新</button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.app-updater {
  min-width: 0;
}
progress {
  width: 100%;
  height: 8px;
  margin: 10px 0 4px;
  accent-color: var(--brand);
}
.update-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin-top: 12px;
}
.update-actions button {
  padding: 8px 12px;
  border-radius: 8px;
}
.release-notes {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  max-height: 180px;
  overflow: auto;
}
</style>
