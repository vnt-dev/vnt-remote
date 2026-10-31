<script setup lang="ts">
import { computed } from 'vue';
import type { RemoteScreen } from '../services/api';
const props = defineProps<{
  screens: RemoteScreen[];
  active: number;
  disabled: boolean;
  detached?: number[];
}>();
const localCount = computed(
  () => props.screens.filter((screen) => !props.detached?.includes(screen.streamId)).length,
);
const emit = defineEmits<{ select: [id: number]; close: [id: number]; detach: [id: number] }>();
</script>

<template>
  <details
    v-if="screens.length"
    class="screen-switcher"
    @pointerdown.stop
    @mousedown.stop
    @dblclick.stop
    @keydown.stop
    @keyup.stop
  >
    <summary>
      {{ screens.find((screen) => screen.streamId === active)?.name || '选择屏幕' }} ▾
    </summary>
    <div class="screen-menu" role="menu" aria-label="远程屏幕">
      <div v-for="screen in screens" :key="screen.streamId" class="screen-row">
        <button
          type="button"
          role="menuitemradio"
          :aria-checked="screen.streamId === active"
          :disabled="disabled"
          :title="screen.reason"
          @click="emit('select', screen.streamId)"
        >
          <span>{{ screen.streamId === active ? '✓' : ' ' }} {{ screen.name }}</span>
          <small>{{
            detached?.includes(screen.streamId)
              ? '独立窗口 · 点击激活'
              : screen.available
                ? `${screen.width} × ${screen.height}`
                : screen.reason || '暂不可用'
          }}</small>
        </button>
        <button
          v-if="localCount > 1 && !detached?.includes(screen.streamId)"
          type="button"
          class="detach-screen"
          :disabled="disabled"
          :aria-label="`将${screen.name}拆分为独立窗口`"
          title="拆分为独立窗口，拆分后不能合并"
          @click="emit('detach', screen.streamId)"
        >
          ↗
        </button>
        <button
          v-if="localCount > 1 && !detached?.includes(screen.streamId)"
          type="button"
          class="close-screen"
          :disabled="disabled"
          :aria-label="`关闭${screen.name}`"
          title="关闭此屏幕，本次会话不能恢复"
          @click="emit('close', screen.streamId)"
        >
          ×
        </button>
      </div>
    </div>
  </details>
</template>

<style scoped>
.screen-switcher {
  position: relative;
  flex-shrink: 0;
  font-size: 12px;
}
summary {
  max-width: 180px;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  padding: 6px 10px;
  border: 1px solid var(--border);
  border-radius: 6px;
  cursor: pointer;
  list-style: none;
}
.screen-menu {
  position: absolute;
  top: calc(100% + 6px);
  left: 0;
  min-width: 240px;
  max-height: 360px;
  overflow: auto;
  z-index: 60;
  background: var(--bg, #181b24);
  color: var(--text);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 4px;
  box-shadow: 0 8px 24px #0004;
}
.screen-row {
  display: flex;
  align-items: stretch;
}
button {
  display: flex;
  flex: 1;
  align-items: flex-start;
  flex-direction: column;
  gap: 3px;
  border: 0;
  border-radius: 4px;
  color: inherit;
  background: transparent;
  padding: 8px;
  cursor: pointer;
  text-align: left;
}
button:hover:enabled {
  background: #80808022;
}
button:disabled {
  opacity: 0.4;
  cursor: default;
}
small {
  padding-left: 20px;
  opacity: 0.65;
}
.detach-screen,
.close-screen {
  flex: 0 0 30px;
  justify-content: center;
  align-items: center;
  font-size: 18px;
}
</style>
