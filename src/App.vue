<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { getCurrentWindow } from '@tauri-apps/api/window';
import Home from './views/Home.vue';
import RemoteView from './views/RemoteView.vue';
import { api, type ClientSessionInfo } from './services/api';

const remote = new URLSearchParams(location.search).has('remote');
const session = ref<ClientSessionInfo | null>(null);
const error = ref('');
const connecting = ref(false);
const peer = ref('');
const allowFallback = ref(false);
async function connect() {
  if (connecting.value) return;
  connecting.value = true;
  error.value = '';
  try {
    if (new URLSearchParams(location.search).has('screen')) {
      const context = await api.screenWindowContext();
      peer.value = context.peer;
      allowFallback.value = false;
      session.value = {
        ...context.session,
        windowStreamId: context.streamId,
        clipboardEnabled: context.clipboardEnabled,
      };
      return;
    }
    const params = await api.controlWindowParams();
    peer.value = params.host;
    allowFallback.value = params.codecs.length > 1 && params.codecs.includes('h264');
    session.value = await api.clientConnect(
      params.host,
      params.port,
      params.password,
      params.codecs,
      params.fps,
      params.bitrateKbps,
      params.transport,
      params.audioEnabled,
      params.audioDeviceId,
      params.resolutionMode,
      params.targetWidth,
      params.targetHeight,
      params.fecEnabled ?? false,
      params.showWallpaper ?? true,
      params.dragFullWindows ?? false,
      params.colorDepth ?? 24,
      params.hardwareCodecs,
      params.serverNodeId,
      params.serverNetworkCode,
      params.serverConfigId,
      params.clipboardEnabled,
    );
  } catch (e) {
    error.value = String(e);
  } finally {
    connecting.value = false;
  }
}
function close() {
  getCurrentWindow()
    .close()
    .catch((e: unknown) => {
      error.value = String(e);
    });
}
onMounted(() => {
  if (remote) connect();
});
</script>

<template>
  <Home v-if="!remote" />
  <RemoteView
    v-else-if="session"
    :key="session.sessionId"
    :session="session"
    :peer="peer"
    :allow-fallback="allowFallback"
    @connected="session = $event"
    @exit="close"
  />
  <div v-else class="connection-state">
    <img class="brand-mark" src="/icon.svg" alt="VNT Remote" />
    <h2>{{ error ? '未能建立连接' : '正在连接远程设备' }}</h2>
    <p class="muted">{{ peer || '远程桌面' }}</p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>
    <p v-else class="muted">正在验证访问权限并协商视频流…</p>
    <div class="actions">
      <button v-if="error" @click="connect">重新连接</button
      ><button class="ghost" @click="close">关闭窗口</button>
    </div>
  </div>
</template>
