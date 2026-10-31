<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { api } from '../services/api';
import type {
  ClientSessionInfo,
  ClientStats,
  ClientFecStatus,
  ClientTraffic,
  ClientAudioStatus,
} from '../services/api';
import { useRemoteDecoder } from '../composables/useRemoteDecoder';
import { useScreenVisibility } from '../composables/useScreenVisibility';
import ScreenSwitcher from '../components/ScreenSwitcher.vue';
import { useInput } from '../composables/useInput';
import { useClipboard } from '../composables/useClipboard';

const props = defineProps<{ session: ClientSessionInfo; allowFallback?: boolean; peer?: string }>();
const emit = defineEmits<{ exit: []; connected: [info: ClientSessionInfo] }>();

const canvasRef = ref<HTMLCanvasElement>();
const stageRef = ref<HTMLElement>();
const closedReason = ref('');
const statsRtt = ref(0);
const serverFps = ref<number | null>(null);
const serverBitrate = ref<number | null>(null);
const showDetails = ref(false);
const adaptiveStats = ref<ClientStats | null>(null);
const traffic = ref<ClientTraffic | null>(null);
const fec = ref<ClientFecStatus | null>(null);
const audio = ref<ClientAudioStatus | null>(null);
const audioAvailable = computed(() => !closedReason.value && !!audio.value?.available);
const audioReason = computed(() =>
  closedReason.value
    ? '连接已断开'
    : audio.value?.reason ||
      (audioAvailable.value ? 'Opus · 48 kHz 立体声 · 64 kbps' : props.session.audioReason),
);

// 自绘标题栏的窗口控制（浏览器预览无 Tauri 运行时，保持空引用即可）
let tauriWin: ReturnType<typeof getCurrentWindow> | null = null;
try {
  tauriWin = getCurrentWindow();
} catch {
  // 纯浏览器预览：没有可用的窗口句柄
}
function winMinimize() {
  tauriWin?.minimize().catch(() => undefined);
}
function winClose() {
  // 关闭窗口：on_window_event 里会清理连接并释放远端按键
  tauriWin?.close().catch(() => undefined);
}

// 全屏状态（原生无边框，setFullscreen）：自绘标题栏隐藏，改用居中浮条。
// 标题栏的「放大」按钮即全屏开关——远程控制窗口不需要普通的窗口最大化
const fullscreen = ref(false);
async function refreshFullscreen() {
  try {
    fullscreen.value = (await tauriWin?.isFullscreen()) ?? false;
  } catch {
    fullscreen.value = false;
  }
}
async function toggleFullscreen() {
  try {
    await tauriWin?.setFullscreen(!fullscreen.value);
    await refreshFullscreen();
  } catch {
    // 浏览器预览无 Tauri 运行时
  }
}
// F11 切换全屏：capture 阶段拦截，不透传给远端
function onKeydown(e: KeyboardEvent) {
  if (e.key !== 'F11') return;
  e.preventDefault();
  e.stopPropagation();
  void toggleFullscreen();
}

// 全屏时浮条自动隐藏：鼠标移到「浮条自身的横向范围」内的顶部边缘才唤出
// （左右两端不触发），离开浮条区域再隐藏；图钉固定后不再自动隐藏。
// 监听必须挂在 window 捕获阶段——画面区 useInput 会 setPointerCapture
// 并把点击转发给远端，组件根上的冒泡监听会被吞掉
const pillRef = ref<HTMLElement>();
const pinned = ref(false);
const revealed = ref(false);
const REVEAL_EDGE = 10;
const REVEAL_SLACK = 20;
/** 唤出时横向命中范围比浮条本身放宽的距离（px） */
const REVEAL_MARGIN = 40;
const pillHidden = computed(() => fullscreen.value && !pinned.value && !revealed.value);
function onGlobalPointerMove(ev: PointerEvent) {
  if (!fullscreen.value || pinned.value || pillRef.value?.querySelector('details[open]')) return;
  const rect = pillRef.value?.getBoundingClientRect();
  const bottom = rect?.bottom ?? 52;
  const left = (rect?.left ?? 0) - REVEAL_MARGIN;
  const right = (rect?.right ?? window.innerWidth) + REVEAL_MARGIN;
  if (!revealed.value && ev.clientY <= REVEAL_EDGE && ev.clientX >= left && ev.clientX <= right) {
    revealed.value = true;
  } else if (revealed.value && ev.clientY > bottom + REVEAL_SLACK) {
    revealed.value = false;
  }
}

// 标题栏拖拽 / 双击全屏：不用 data-tauri-drag-region（其双击固定为
// 最大化），手动 startDragging，双击行为与「放大」按钮一致进入全屏。
// 仅 detail===1 启动拖拽，避免吃掉双击的第二次按下
function onCtbMouseDown(e: MouseEvent) {
  if ((e.target as HTMLElement).closest('button, select, details, input')) return;
  if (e.detail === 1) tauriWin?.startDragging().catch(() => undefined);
}
function onCtbDoubleClick(e: MouseEvent) {
  if ((e.target as HTMLElement).closest('button, select, details, input')) return;
  void toggleFullscreen();
}

const decoder = useRemoteDecoder(canvasRef, props.session, recoverVideo);
if (tauriWin) onUnmounted(useScreenVisibility(props.session.sessionId, decoder.inputTarget));
const windowTitle = computed(() =>
  [
    props.peer || '远程桌面',
    props.session.windowStreamId
      ? decoder.screens.value.find((s) => s.streamId === props.session.windowStreamId)?.name
      : '',
  ]
    .filter(Boolean)
    .join(' · '),
);
const input = useInput(
  () => canvasRef.value,
  props.session.sessionId,
  () => decoder.inputTarget.value,
);
// 被控端关闭「允许控制」时：键鼠不绑定不转发，剪贴板同步禁用
const viewOnly = computed(() => props.session.viewOnly);
const canSendSas = computed(() => !viewOnly.value && props.session.hostEdition === 'installed');
const sasError = ref('');
const remoteNotice = computed(
  () =>
    sasError.value ||
    (decoder.desktop.value.ready ? decoder.desktop.value.reason : '') ||
    props.session.resolutionReason,
);
async function sendSas() {
  if (!canSendSas.value) return;
  sasError.value = '';
  try {
    await api.clientSendSas(props.session.sessionId);
  } catch (error) {
    sasError.value = String(error);
  }
}
const desktopWaiting = computed(() => !decoder.desktop.value.ready || !decoder.frameReady.value);
const desktopHint = ref(false);
const desktopDetail = ref(false);
let desktopHintTimer: ReturnType<typeof setTimeout> | undefined;
let desktopDetailTimer: ReturnType<typeof setTimeout> | undefined;
function clearDesktopTimers() {
  clearTimeout(desktopHintTimer);
  clearTimeout(desktopDetailTimer);
}
watch(
  () => desktopWaiting.value && !closedReason.value,
  (waiting) => {
    clearDesktopTimers();
    desktopHint.value = false;
    desktopDetail.value = false;
    if (waiting) {
      desktopHintTimer = setTimeout(() => {
        desktopHint.value = true;
      }, 600);
      desktopDetailTimer = setTimeout(() => {
        desktopDetail.value = true;
      }, 3000);
    }
  },
  { immediate: true },
);
watch(
  () => !desktopWaiting.value && decoder.transport.value.control && !decoder.busy.value,
  (ready) => {
    input.stop();
    if (ready && stageRef.value && !viewOnly.value && !closedReason.value)
      input.bind(stageRef.value);
  },
);
watch(
  () => [decoder.activeStreamId.value, decoder.revision.value],
  () => {
    input.stop();
    if (
      !desktopWaiting.value &&
      decoder.transport.value.control &&
      !decoder.busy.value &&
      stageRef.value &&
      !viewOnly.value &&
      !closedReason.value
    )
      input.bind(stageRef.value);
  },
);
watch(decoder.screenStats, (stats) => {
  if (!stats) return;
  serverFps.value = stats.serverFps;
  serverBitrate.value = stats.serverBitrateKbps;
  adaptiveStats.value = { ...stats, rttMs: statsRtt.value };
});
const clipboard = useClipboard(
  props.session.sessionId,
  props.session.clipboardConnectionId,
  viewOnly.value ? false : props.session.clipboardEnabled,
  !!props.session.windowStreamId,
);

const streamWidth = computed(() => decoder.streamInfo.value?.width ?? props.session.width);
const streamHeight = computed(() => decoder.streamInfo.value?.height ?? props.session.height);
const streamFps = computed(() => decoder.streamInfo.value?.fps ?? props.session.fps);
const liveFps = computed(() => (closedReason.value ? 0 : decoder.stats.value.displayedFps));
function formatTrafficSpeed(bytesPerSecond: number) {
  return bytesPerSecond >= 1_000_000
    ? `${(bytesPerSecond / 1_000_000).toFixed(1)} MB/s`
    : `${Math.round(bytesPerSecond / 1000)} KB/s`;
}
const receiveSpeed = computed(() =>
  formatTrafficSpeed(closedReason.value ? 0 : (traffic.value?.downloadBytesPerSecond ?? 0)),
);
const uploadSpeed = computed(() =>
  formatTrafficSpeed(closedReason.value ? 0 : (traffic.value?.uploadBytesPerSecond ?? 0)),
);

const unlisteners: UnlistenFn[] = [];
let disposed = false;
let retryingVideo = false;
async function recoverVideo(reason: string) {
  if (
    props.session.windowStreamId ||
    props.session.codec === 'h264' ||
    props.allowFallback === false
  ) {
    finish(reason);
    return;
  }
  if (retryingVideo || disposed) return;
  retryingVideo = true;
  input.stop();
  closedReason.value = '当前编码无法解码，正在切换 H264…';
  try {
    const info = await api.clientRetryH264(props.session.sessionId);
    if (disposed) {
      await api.clientDisconnect(info.sessionId);
      return;
    }
    emit('connected', info);
  } catch (error) {
    if (!disposed) finish(`${reason}；H264 重连失败：${error}`);
  }
}
function register(unlisten: UnlistenFn) {
  if (disposed) unlisten();
  else unlisteners.push(unlisten);
}
function finish(reason: string) {
  closedReason.value = reason;
  input.stop();
  decoder.close();
  api.clientDisconnect(props.session.sessionId).catch(() => undefined);
}

// 观看模式：画面区不转发键鼠，右键也不弹本地菜单
function onStageContextMenu(e: Event) {
  if (props.session.viewOnly) e.preventDefault();
}

onMounted(() => {
  // Bind before connecting: synchronous decoder setup failure must be able to
  // remove these listeners and prevent input on the error overlay.
  if (
    stageRef.value &&
    !props.session.viewOnly &&
    !desktopWaiting.value &&
    decoder.transport.value.control
  )
    input.bind(stageRef.value);
  stageRef.value?.focus();
  decoder.connect(props.session.wsPort, props.session.token, {
    codec: props.session.codec,
    width: props.session.width,
    height: props.session.height,
    fps: props.session.fps,
    hardware: props.session.hardware,
  });
  void clipboard.subscribe();

  listen<ClientFecStatus>('client://fec', (ev) => {
    if (ev.payload.sessionId === props.session.sessionId) fec.value = ev.payload;
  })
    .then(register)
    .catch(() => undefined);
  listen<ClientAudioStatus>('client://audio', (ev) => {
    if (ev.payload.sessionId === props.session.sessionId) audio.value = ev.payload;
  })
    .then(register)
    .catch(() => undefined);

  listen<ClientTraffic>('client://traffic', (ev) => {
    if (
      ev.payload.sessionId !== props.session.sessionId ||
      ev.payload.connectionId !== props.session.clipboardConnectionId
    )
      return;
    traffic.value = ev.payload;
  })
    .then(register)
    .catch((error: unknown) => finish(String(error)));

  listen<ClientStats>('client://stats', (ev) => {
    if (ev.payload.sessionId !== props.session.sessionId) return;
    statsRtt.value = ev.payload.rttMs;
    serverFps.value = (decoder.screenStats.value ?? ev.payload).serverFps;
    serverBitrate.value = (decoder.screenStats.value ?? ev.payload).serverBitrateKbps;
    adaptiveStats.value = { ...(decoder.screenStats.value ?? ev.payload), rttMs: ev.payload.rttMs };
  })
    .then(register)
    .catch((error: unknown) => finish(String(error)));
  listen<{ reason: string; sessionId: number }>('client://closed', (ev) => {
    if (ev.payload.sessionId !== props.session.sessionId) return;
    if (!retryingVideo) finish(ev.payload.reason || '连接已断开');
  })
    .then(register)
    .catch((error: unknown) => finish(String(error)));

  void refreshFullscreen();
  document.addEventListener('keydown', onKeydown, true);
  window.addEventListener('pointermove', onGlobalPointerMove, true);
  try {
    getCurrentWindow()
      .onResized(() => {
        void refreshFullscreen();
      })
      .then(register)
      .catch(() => undefined);
  } catch {
    // 浏览器预览：没有 Tauri 窗口事件可订阅
  }
});

onUnmounted(() => {
  clearDesktopTimers();
  disposed = true;
  unlisteners.forEach((fn) => fn());
  document.removeEventListener('keydown', onKeydown, true);
  window.removeEventListener('pointermove', onGlobalPointerMove, true);
  decoder.close();
  api.clientDisconnect(props.session.sessionId).catch(() => undefined);
});
</script>

<template>
  <svg width="0" height="0" style="position: absolute" aria-hidden="true">
    <defs>
      <symbol id="rw-logo" viewBox="0 0 24 24">
        <image href="/icon.svg" width="24" height="24" />
      </symbol>
      <symbol id="rw-info" viewBox="0 0 24 24">
        <circle cx="12" cy="12" r="8.5" />
        <path d="M12 11v5M12 8h.01" />
      </symbol>
      <symbol id="rw-pin" viewBox="0 0 24 24">
        <path d="M12 16.5v4.5" />
        <path d="M8.2 3.5h7.6l-.7 6.2 2.4 3.3H6.5l2.4-3.3-.7-6.2Z" />
      </symbol>
      <symbol id="rw-shrink" viewBox="0 0 24 24">
        <path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7" />
      </symbol>
      <symbol id="rw-min" viewBox="0 0 24 24"><path d="M5.5 12h13" /></symbol>
      <symbol id="rw-max" viewBox="0 0 24 24">
        <rect x="6" y="6" width="12" height="12" rx="1.5" />
      </symbol>
      <symbol id="rw-x" viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18" /></symbol>
    </defs>
  </svg>

  <div class="remote" :class="{ fullscreen }" tabindex="0">
    <!-- 自绘标题栏：连接信息合并于此（窗口化；全屏时隐藏，改用居中浮条）。
         拖拽/双击由 onCtbMouseDown / onCtbDoubleClick 处理 -->
    <header
      v-show="!fullscreen"
      class="ctb"
      @mousedown="onCtbMouseDown"
      @dblclick="onCtbDoubleClick"
    >
      <div class="peer">
        <svg width="16" height="16"><use href="#rw-logo" /></svg>
        <span class="dot"></span>
        <span class="pn num">{{ windowTitle }}</span>
      </div>
      <ScreenSwitcher
        v-if="!session.windowStreamId"
        :screens="decoder.screens.value"
        :detached="decoder.detachedScreens.value"
        :active="decoder.activeStreamId.value"
        :disabled="decoder.busy.value || !decoder.transport.value.control || !!closedReason"
        @select="decoder.select"
        @close="decoder.closeScreen"
        @detach="decoder.detach"
      />
      <div class="metrics num">
        <span v-if="viewOnly" class="rchip view" title="被控端未允许控制：本窗口只能观看">
          观看模式
        </span>
        <span class="rchip ok chip-codec">
          <b>{{ session.codec.toUpperCase() }}</b> ·
          {{ session.hardware ? '硬件编码' : '软件编码' }}
        </span>
        <span class="live-fps" title="每秒实际显示帧数"
          ><b>{{ liveFps }}</b> fps</span
        >
        <span class="live-speed" title="总下载：视频、音频、文件、剪贴板及控制消息，每秒更新"
          >↓ {{ receiveSpeed }}</span
        >
        <span class="live-speed" title="总上传：文件、剪贴板及控制消息，每秒更新"
          >↑ {{ uploadSpeed }}</span
        >
        <span class="rchip num chip-rtt"
          >RTT <b>{{ statsRtt }} ms</b></span
        >
        <span v-if="!decoder.transport.value.control" class="rchip warn"
          >控制连接恢复中（最长 30 秒）</span
        >
        <span
          v-if="
            (props.session.capabilities === undefined || (props.session.capabilities & 4) !== 0) &&
            !decoder.transport.value.files
          "
          class="rchip warn"
          >文件连接恢复中</span
        >
        <span v-if="!decoder.transport.value.media" class="rchip warn">音视频连接恢复中</span>
        <span
          v-if="decoder.lastError.value"
          class="rchip warn decoder-error"
          :title="decoder.lastError.value"
          role="status"
          >{{ decoder.lastError.value }}</span
        >
      </div>
      <span class="grow"></span>
      <button
        v-if="canSendSas"
        class="rbtn"
        type="button"
        title="打开被控电脑的 Windows 安全界面"
        :disabled="!decoder.transport.value.control"
        @click="sendSas"
      >
        Ctrl+Alt+Del
      </button>
      <span
        v-if="remoteNotice"
        class="rchip warn remote-notice"
        :title="remoteNotice"
        role="status"
        >{{ remoteNotice }}</span
      >
      <button
        class="rbtn details-toggle"
        :class="{ on: showDetails }"
        :aria-expanded="showDetails"
        type="button"
        @click="showDetails = !showDetails"
      >
        <svg class="ic sm"><use href="#rw-info" /></svg> 详情
      </button>
      <div class="ctb-ctrl">
        <button type="button" aria-label="最小化" @click="winMinimize">
          <svg class="ic sm"><use href="#rw-min" /></svg>
        </button>
        <button
          type="button"
          aria-label="全屏 (F11)"
          title="全屏 / 退出全屏 (F11)"
          @click="toggleFullscreen"
        >
          <svg class="ic sm"><use href="#rw-max" /></svg>
        </button>
        <button type="button" class="close" aria-label="关闭" @click="winClose">
          <svg class="ic sm"><use href="#rw-x" /></svg>
        </button>
      </div>
    </header>

    <div ref="stageRef" class="stage" tabindex="0" @contextmenu="onStageContextMenu">
      <canvas ref="canvasRef"></canvas>

      <!-- 全屏浮条：自动隐藏，顶部边缘唤出；指标样式与标题栏一致。
           stop 全部冒泡：stage 上的监听会把点击/滚轮转发给远端 -->
      <div
        v-show="fullscreen"
        ref="pillRef"
        class="rtool"
        :class="{ hidden: pillHidden }"
        @pointerdown.stop
        @pointerup.stop
        @mousedown.stop
        @mouseup.stop
        @pointermove.stop
        @wheel.stop
        @dblclick.stop
        @click.stop
        @keydown.stop
        @keyup.stop
        @contextmenu.stop
      >
        <div class="peer">
          <span class="dot"></span>
          <span class="pn num">{{ windowTitle }}</span>
        </div>
        <ScreenSwitcher
          v-if="!session.windowStreamId"
          :screens="decoder.screens.value"
          :detached="decoder.detachedScreens.value"
          :active="decoder.activeStreamId.value"
          :disabled="decoder.busy.value || !decoder.transport.value.control || !!closedReason"
          @select="decoder.select"
          @close="decoder.closeScreen"
          @detach="decoder.detach"
        />
        <div class="metrics num">
          <span v-if="viewOnly" class="rchip view">观看模式</span>
          <span class="live-fps" title="每秒实际显示帧数"
            ><b>{{ liveFps }}</b> fps</span
          >
          <span class="live-speed" title="总下载：视频、音频、文件、剪贴板及控制消息，每秒更新"
            >↓ {{ receiveSpeed }}</span
          >
          <span class="live-speed" title="总上传：文件、剪贴板及控制消息，每秒更新"
            >↑ {{ uploadSpeed }}</span
          >
          <span class="rchip num chip-rtt"
            >RTT <b>{{ statsRtt }} ms</b></span
          >
        </div>
        <span class="grow"></span>
        <button
          v-if="canSendSas"
          class="rbtn"
          type="button"
          title="打开被控电脑的 Windows 安全界面"
          :disabled="!decoder.transport.value.control"
          @click="sendSas"
        >
          Ctrl+Alt+Del
        </button>
        <button
          class="rbtn details-toggle"
          :class="{ on: showDetails }"
          :aria-expanded="showDetails"
          type="button"
          @click="showDetails = !showDetails"
        >
          <svg class="ic sm"><use href="#rw-info" /></svg> 详情
        </button>
        <button
          class="rbtn icon pin"
          :class="{ on: pinned }"
          title="固定顶部：不再自动隐藏"
          aria-label="固定顶部"
          :aria-pressed="pinned"
          type="button"
          @click="pinned = !pinned"
        >
          <svg class="ic sm"><use href="#rw-pin" /></svg>
        </button>
        <button class="rbtn" type="button" @click="toggleFullscreen">
          <svg class="ic sm"><use href="#rw-shrink" /></svg> 退出全屏
        </button>
        <button
          class="rbtn icon close"
          title="直接关闭窗口"
          aria-label="直接关闭窗口"
          type="button"
          @click="winClose"
        >
          <svg class="ic sm"><use href="#rw-x" /></svg>
        </button>
      </div>

      <div
        v-if="fullscreen && remoteNotice"
        class="fullscreen-notice"
        role="status"
        aria-live="polite"
      >
        {{ remoteNotice }}
      </div>

      <!-- 连接详情透明浮窗。stop 防止事件透传到远端 -->
      <div
        v-if="showDetails"
        class="rpanel"
        :class="{ low: fullscreen && !pillHidden }"
        @pointerdown.stop
        @pointerup.stop
        @mousedown.stop
        @mouseup.stop
        @pointermove.stop
        @wheel.stop
        @dblclick.stop
        @click.stop
        @keydown.stop
        @keyup.stop
        @contextmenu.stop
      >
        <div class="rp-head">
          <div class="t">连接详情</div>
          <button class="x" type="button" aria-label="关闭浮窗" @click="showDetails = false">
            <svg class="ic sm"><use href="#rw-x" /></svg>
          </button>
        </div>
        <div class="rp-body">
          <dl class="dl">
            <div>
              <dt>传输方式</dt>
              <dd>
                {{
                  session.transport === 'vnt'
                    ? 'VNT 通道'
                    : session.transport === 'quic'
                      ? 'QUIC'
                      : session.transport === 'tcp'
                        ? '纯 TCP'
                        : 'TCP / UDP 混合'
                }}
              </dd>
            </div>
            <div>
              <dt>自适应 FEC</dt>
              <dd>{{ session.fecEnabled ? '已启用' : session.fecReason || '未启用' }}</dd>
            </div>
            <div v-if="session.fecEnabled">
              <dt>FEC 冗余目标</dt>
              <dd>{{ ((fec?.ratioPermille ?? 50) / 10).toFixed(1) }}%</dd>
            </div>
            <div v-if="session.fecEnabled">
              <dt>原始丢包率</dt>
              <dd>
                {{
                  fec?.rawLossPermille == null
                    ? '等待统计'
                    : `${(fec.rawLossPermille / 10).toFixed(1)}%`
                }}
              </dd>
            </div>
            <div v-if="session.fecEnabled">
              <dt>FEC 已恢复</dt>
              <dd>{{ fec?.recovered ?? 0 }} 个数据片</dd>
            </div>
            <div>
              <dt>系统声音</dt>
              <dd :class="{ ok: audioAvailable }" :title="audioReason">
                {{ audioAvailable ? 'Opus · 48 kHz · 64 kbps' : audioReason }}
              </dd>
            </div>
            <div v-if="audioAvailable && audio?.reason">
              <dt>音频提示</dt>
              <dd>{{ audio.reason }}</dd>
            </div>
            <div v-if="audioAvailable">
              <dt>音频缓冲</dt>
              <dd class="num">{{ audio?.bufferMs ?? 0 }} ms</dd>
            </div>
            <div>
              <dt>控制权限</dt>
              <dd :class="{ ok: !viewOnly }">
                {{ viewOnly ? '仅观看（被控端限制）' : '键鼠控制' }}
              </dd>
            </div>
            <div>
              <dt>编码器</dt>
              <dd class="num">{{ session.encoderName }}</dd>
            </div>
            <div>
              <dt>分辨率</dt>
              <dd class="num">{{ streamWidth }} × {{ streamHeight }} @ {{ streamFps }} fps</dd>
            </div>
            <div>
              <dt>画面帧率</dt>
              <dd
                class="ok num"
                :title="`解码输出 ${decoder.stats.value.fps} fps，其中 ${decoder.stats.value.droppedDecoded} 帧未上屏`"
              >
                {{ decoder.stats.value.displayedFps }} fps <span class="sub">上屏</span>
              </dd>
            </div>
            <div v-if="adaptiveStats?.targetFps != null">
              <dt>目标</dt>
              <dd class="num">
                {{ adaptiveStats.targetFps }} fps · {{ adaptiveStats.targetBitrateKbps }} kbps
              </dd>
            </div>
            <div v-if="adaptiveStats?.appliedFps != null">
              <dt>已应用</dt>
              <dd class="num">
                {{ adaptiveStats.appliedFps }} fps · {{ adaptiveStats.appliedBitrateKbps }} kbps
              </dd>
            </div>
            <div v-if="serverFps !== null">
              <dt>实际发送</dt>
              <dd class="num">{{ serverFps }} fps · {{ serverBitrate }} kbps</dd>
            </div>
            <div v-if="adaptiveStats?.captureMs != null">
              <dt>抓屏耗时</dt>
              <dd class="num">{{ adaptiveStats.captureMs }} ms</dd>
            </div>
            <div v-if="adaptiveStats?.encodeMs != null">
              <dt>编码耗时</dt>
              <dd class="num">{{ adaptiveStats.encodeMs }} ms</dd>
            </div>
            <div v-if="adaptiveStats?.lossPermille != null">
              <dt>丢帧率</dt>
              <dd class="num">{{ (adaptiveStats.lossPermille / 10).toFixed(1) }}%</dd>
            </div>
            <div>
              <dt>缓冲</dt>
              <dd class="num">{{ decoder.stats.value.lagMs }} ms</dd>
            </div>
            <div>
              <dt>上屏 P95</dt>
              <dd class="num">{{ decoder.stats.value.p95Ms }} ms</dd>
            </div>
          </dl>
        </div>
      </div>

      <Transition name="desktop-veil">
        <div
          v-if="!closedReason && desktopWaiting"
          class="overlay desktop-wait"
          @pointerdown.stop
          @pointerup.stop
          @mousedown.stop
          @mouseup.stop
          @wheel.stop
          @keydown.stop
          @keyup.stop
        >
          <Transition name="desktop-hint">
            <div v-if="desktopHint" class="desktop-wait-hint" role="status" aria-live="polite">
              <span>{{
                desktopDetail
                  ? decoder.desktop.value.reason || '画面恢复中，请稍候…'
                  : '正在切换桌面…'
              }}</span>
              <button
                v-if="desktopDetail && canSendSas"
                class="desktop-sas"
                type="button"
                @click="sendSas"
              >
                发送 Ctrl+Alt+Del
              </button>
            </div>
          </Transition>
        </div>
      </Transition>
      <div
        v-if="closedReason"
        class="overlay err"
        @pointerdown.stop
        @pointerup.stop
        @mousedown.stop
        @mouseup.stop
        @keydown.stop
        @keyup.stop
      >
        <div>{{ closedReason }}</div>
        <button class="overlay-btn" type="button" @click="emit('exit')">关闭窗口</button>
      </div>

      <div
        v-if="input.captureError.value || input.unsupportedKey.value"
        class="rhint"
        role="status"
      >
        {{ input.captureError.value || input.unsupportedKey.value }}
      </div>
    </div>
  </div>
</template>

<style scoped>
/* 控制窗口恒定深色玻璃（不随应用主题变化），token 局部定义 */
.remote {
  width: 100%;
  min-width: 0;
  overflow: hidden;
  height: 100%;
  display: flex;
  flex-direction: column;
  position: relative;
  outline: none;
  background: #0a0d13;
  color: #e9edf6;
  --rw-line: rgba(255, 255, 255, 0.09);
  --rw-text3: #67718a;
  --rw-brand: #7d95ff;
  --rw-ok: #3ecf8e;
}
.remote :focus-visible {
  outline-color: var(--rw-brand);
}

/* ---------- 自绘标题栏（窗口化） ---------- */
.ctb {
  height: 40px;
  flex: none;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 0 0 0 14px;
  background: #14181f;
  border-bottom: 1px solid #0e1116;
  user-select: none;
  cursor: default;
}
.ctb .peer {
  display: flex;
  align-items: center;
  gap: 9px;
  min-width: 0;
}
.ctb .pn {
  font-size: 12.5px;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ctb .grow {
  flex: 1;
  align-self: stretch;
}
.ctb-ctrl {
  display: flex;
  align-self: stretch;
}
.ctb-ctrl button {
  width: 42px;
  border: 0;
  background: transparent;
  color: #aab3c5;
  display: grid;
  place-items: center;
  padding: 0;
  cursor: default;
  border-radius: 0;
}
.ctb-ctrl button:hover {
  background: rgba(255, 255, 255, 0.08);
  color: #fff;
}
.ctb-ctrl button.close:hover {
  background: #e81123;
  color: #fff;
}

/* ---------- 通用小件 ---------- */
.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--rw-ok);
  position: relative;
  flex: none;
}
.dot::after {
  content: '';
  position: absolute;
  inset: -4px;
  border-radius: 50%;
  border: 1.5px solid var(--rw-ok);
  opacity: 0.45;
  animation: rw-pulse 2.2s ease-out infinite;
}
@keyframes rw-pulse {
  0% {
    transform: scale(0.55);
    opacity: 0.7;
  }
  75%,
  100% {
    transform: scale(1.25);
    opacity: 0;
  }
}
.metrics {
  min-width: 0;
  overflow: hidden;
  display: flex;
  align-items: center;
  gap: 10px;
  flex: 0 1 auto;
  font-size: 11.5px;
  color: #98a2b8;
  white-space: nowrap;
  font-variant-numeric: tabular-nums;
}
.metrics > span {
  flex-shrink: 0;
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding-left: 12px;
}
.metrics > span::before {
  content: '|';
  position: absolute;
  left: 0;
  color: var(--rw-text3);
}
.metrics b {
  color: #e9edf6;
  font-weight: 600;
}
.metrics > .decoder-error {
  flex-shrink: 1;
  min-width: 0;
  max-width: min(28vw, 360px);
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
}
.rchip.warn {
  color: #f5c26b;
}
.remote-notice {
  max-width: min(28vw, 320px);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.fullscreen-notice {
  position: absolute;
  z-index: 30;
  top: 64px;
  left: 50%;
  transform: translateX(-50%);
  max-width: min(680px, calc(100% - 40px));
  padding: 10px 14px;
  border-radius: 8px;
  background: rgba(20, 25, 35, 0.94);
  color: #f5c26b;
  font-size: 13px;
  overflow-wrap: anywhere;
  pointer-events: none;
}
/* 观看模式和告警仅保留文字颜色。 */
.rchip.view {
  color: #9db0ff;
}
/* 数值固定宽度，刷新时避免标题栏与浮条抖动。 */
.live-fps {
  min-width: 60px;
}
.live-speed {
  min-width: 100px;
}
.chip-rtt {
  min-width: 86px;
}
@media (max-width: 1000px) {
  .ctb .chip-codec {
    display: none;
  }
}
.rbtn {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  height: 31px;
  padding: 0 12px;
  border-radius: 8px;
  border: 1px solid transparent;
  background: transparent;
  color: #98a2b8;
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
}
.rbtn:hover {
  background: rgba(255, 255, 255, 0.07);
  color: #e9edf6;
}
.rbtn.on {
  background: rgba(125, 149, 255, 0.14);
  border-color: rgba(125, 149, 255, 0.3);
  color: var(--rw-brand);
}
.rbtn.icon {
  width: 31px;
  padding: 0;
  justify-content: center;
}
.rbtn.close:hover {
  background: rgba(232, 17, 35, 0.2);
  color: #ff7b7e;
}
.pin.on svg {
  transform: rotate(45deg);
}
.pin svg {
  transition: transform 0.15s;
}

/* ---------- 画面区（铺满整窗） ---------- */
.stage {
  min-width: 0;
  overflow: hidden;
  flex: 1;
  min-height: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  position: relative;
  background: #000;
}
.stage canvas {
  max-width: 100%;
  max-height: 100%;
  background: #000;
  outline: none;
}

/* ---------- 全屏居中浮条 ---------- */
.rtool {
  position: absolute;
  top: 0;
  left: 50%;
  transform: translateX(-50%);
  z-index: 5;
  display: flex;
  align-items: center;
  gap: 8px;
  height: 42px;
  padding: 0 8px 0 14px;
  border-radius: 0 0 14px 14px;
  background: rgba(13, 17, 25, 0.72);
  backdrop-filter: blur(18px);
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-top: 0;
  box-shadow: 0 12px 40px rgba(0, 0, 0, 0.35);
  transition:
    transform 0.18s ease,
    opacity 0.18s ease;
}
.rtool .peer {
  display: flex;
  align-items: center;
  gap: 9px;
  padding-right: 4px;
}
.rtool .pn {
  font-size: 13px;
  font-weight: 600;
}
.rtool .grow {
  flex: 1;
}
.rtool.hidden {
  opacity: 0;
  transform: translateX(-50%) translateY(-14px);
  pointer-events: none;
}

/* ---------- 连接详情透明浮窗 ---------- */
.rpanel {
  position: absolute;
  top: 12px;
  right: 12px;
  width: 312px;
  z-index: 6;
  border-radius: 14px;
  overflow: hidden;
  background: rgba(12, 16, 24, 0.72);
  backdrop-filter: blur(22px);
  border: 1px solid rgba(255, 255, 255, 0.12);
  box-shadow: 0 24px 60px -12px rgba(0, 0, 0, 0.55);
}
/* 全屏且浮条可见时，浮窗下移避让 */
.rpanel.low {
  top: 64px;
}
.rp-head {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 12px 14px 9px;
}
.rp-head .t {
  flex: 1;
  font-size: 13px;
  font-weight: 600;
}
.rp-head .x {
  width: 26px;
  height: 26px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--rw-text3);
  display: grid;
  place-items: center;
  cursor: pointer;
  padding: 0;
}
.rp-head .x:hover {
  background: rgba(255, 255, 255, 0.08);
  color: #e9edf6;
}
.rp-body {
  padding: 0 14px 12px;
}
.dl {
  margin: 0;
}
.dl > div {
  display: flex;
  align-items: baseline;
  gap: 10px;
  padding: 6.5px 2px;
  border-bottom: 1px solid rgba(255, 255, 255, 0.055);
}
.dl > div:last-child {
  border-bottom: 0;
}
.dl dt {
  flex: none;
  font-size: 11.5px;
  color: var(--rw-text3);
}
.dl dd {
  margin: 0;
  flex: 1;
  font-size: 12.5px;
  text-align: right;
  font-variant-numeric: tabular-nums;
}
.dl dd .sub {
  color: var(--rw-text3);
  font-size: 11px;
}
.dl dd.ok {
  color: var(--rw-ok);
}

/* ---------- 遮罩与提示 ---------- */
.overlay {
  position: absolute;
  inset: 0;
  z-index: 4;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  background: rgba(5, 7, 13, 0.72);
  color: #e6ebf5;
  font-size: 15px;
  padding: 24px;
  text-align: center;
}
.overlay.desktop-wait {
  background: #080b10;
  padding: 20px;
  justify-content: flex-end;
}
.desktop-wait-hint {
  display: flex;
  align-items: center;
  gap: 12px;
  max-width: 640px;
  color: #8c96a8;
  font-size: 12px;
}
.desktop-sas {
  flex: none;
  padding: 4px 8px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 5px;
  background: transparent;
  color: #aab4c5;
  font: inherit;
  cursor: pointer;
}
.desktop-veil-leave-active,
.desktop-hint-enter-active,
.desktop-hint-leave-active {
  transition: opacity 140ms ease;
}
.desktop-veil-leave-active {
  pointer-events: none;
}
.desktop-veil-leave-to,
.desktop-hint-enter-from,
.desktop-hint-leave-to {
  opacity: 0;
}
@media (prefers-reduced-motion: reduce) {
  .desktop-veil-leave-active,
  .desktop-hint-enter-active,
  .desktop-hint-leave-active {
    transition: none;
  }
}
.overlay.err {
  color: #ff8a8e;
}
.overlay-btn {
  margin-top: 16px;
  height: 36px;
  padding: 0 16px;
  border-radius: 9px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(255, 255, 255, 0.06);
  color: #e9edf6;
  font-weight: 600;
  font-size: 13px;
  cursor: pointer;
}
.overlay-btn:hover {
  background: rgba(255, 255, 255, 0.12);
}
.rhint {
  position: absolute;
  left: 18px;
  bottom: 18px;
  z-index: 3;
  max-width: 460px;
  color: rgba(255, 255, 255, 0.5);
  font-size: 11.5px;
  pointer-events: none;
  text-shadow: 0 1px 3px rgba(0, 0, 0, 0.5);
}
</style>
