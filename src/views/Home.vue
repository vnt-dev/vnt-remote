<script setup lang="ts">
import {
  parseDevices,
  writeDevices,
  type DeviceAdvanced,
  type SavedDevice,
} from '../services/devices';
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, watch } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { getVersion } from '@tauri-apps/api/app';
import { api, type PlatformInfo } from '../services/api';
import { edition, loadEdition } from '../services/edition';
import { createHostConfigSaver } from '../services/hostConfig';
import { ADDRESS_ERROR, DEFAULT_PORT, parseAddr, formatAddr } from '../services/address';
import { formatConnectionShare, parseConnectionShare } from '../services/connectionShare';
import {
  fitResolution,
  resolutionOptions,
  resolutionTarget,
  SCREEN_RESOLUTION,
  type ScreenSize,
} from '../services/resolution';
import AppSelect from '../components/AppSelect.vue';
import AppUpdater from '../components/AppUpdater.vue';
import ScreenRegions from '../components/ScreenRegions.vue';
import ServerSettings from '../components/ServerSettings.vue';
import { linkPhaseLabel, syncDeviceGroups, connectionIdLabel } from '../services/linkProfiles';
import type { ProfileStatus, OnlineDevice } from '../services/api';
import { useDeviceSort } from '../composables/useDeviceSort';
import type {
  AppConfig,
  AudioPlaybackDevice,
  MonitorInfo,
  ServerSessionInfo,
  Transport,
} from '../services/api';
import {
  AUDIO_PLAYBACK_DISABLED,
  connectionAudio,
  resolveAudioPlaybackDevice,
} from '../services/audio';
import {
  loadHwDecodePref,
  probeCodecSupport,
  saveHwDecodePref,
  supportedCodecList,
  supportedHardwareCodecList,
  type CodecSupport,
} from '../services/codec';
import {
  loadColorDepthPref,
  saveColorDepthPref,
  loadShowWallpaperPref,
  saveShowWallpaperPref,
  loadDragFullWindowsPref,
  saveDragFullWindowsPref,
  loadTargetResolutionPref,
  saveTargetResolutionPref,
  loadResolutionModePref,
  saveResolutionModePref,
  loadClipboardSyncPref,
  saveClipboardSyncPref,
  loadAudioPlaybackPref,
  saveAudioPlaybackPref,
} from '../services/prefs';
import { setThemeMode, themeMode, type ThemeMode } from '../services/theme';

const cfg = reactive<AppConfig>({
  password: '',
  passwordConfigured: false,
  servers: [],
  port: 29875,
  formatVersion: 1,
  selectedTargets: [],
  screenRegions: [],
  clipboard: true,
  audio: true,
  hardwareEncode: true,
  captureBorder: false,
  allowControl: true,
});
const monitors = ref<MonitorInfo[]>([]);
const serverRunning = ref(false);
const serviceInstalled = ref(false);
const serviceState = ref('unknown');
const portLocked = computed(
  () => serverRunning.value || ['starting', 'stopping'].includes(serviceState.value),
);
const autoStart = ref(false);
const serviceError = ref('');
const configSaveError = ref('');
const persistConfig = createHostConfigSaver(cfg, {
  getConfig: api.getConfig,
  setConfig: api.setConfig,
});
const serviceLabel = computed(
  () =>
    ({
      notInstalled: '请先安装程序',
      stopped: '已停止',
      starting: '启动中',
      stopping: '停止中',
      running: '运行中',
      fault: '服务故障',
      unknown: '正在查询服务',
    })[serviceState.value] ?? serviceState.value,
);
let statusTimer: ReturnType<typeof setInterval> | undefined;

const serverPort = ref(0);
const sessions = ref<ServerSessionInfo[]>([]);
const platform = ref<PlatformInfo | null>(null);
const requestingPermission = ref(false);
const codecSupport = ref<CodecSupport>({ h264: true, hevc: false, av1: false });

// 连接表单：地址端口合并输入（host[:port]，端口缺省 29875）
const addr = ref('');
const selectedServerConfigId = ref('');
const selectedServerNodeId = ref('');
const selectedServerGroup = ref('');
const linkStatuses = ref<ProfileStatus[]>([]);
const linkGroups = computed(() => syncDeviceGroups(linkStatuses.value));
let linkTimer: ReturnType<typeof setInterval> | undefined;
let linkRefreshing = false;
async function refreshLink() {
  if (linkRefreshing) return;
  linkRefreshing = true;
  try {
    linkStatuses.value = await api.serverLinkStatus();
  } catch {
    linkStatuses.value = linkStatuses.value.map((s) => ({ ...s, phase: 'error', devices: [] }));
  } finally {
    linkRefreshing = false;
  }
}
async function linkSaved() {
  await refreshLink();
  Object.assign(cfg, await api.getConfig());
}
function useOnlineDevice(device: OnlineDevice, group: ProfileStatus) {
  selectedServerConfigId.value = group.configId;
  selectedServerNodeId.value = device.id;
  selectedServerGroup.value = group.networkCode;
  selectedDeviceId.value = '';
  addr.value = connectionIdLabel(device.connectionId);
  password.value = '';
  ddOpen.value = false;
}
function clearServerSelection() {
  selectedServerConfigId.value = '';
  selectedServerNodeId.value = '';
  selectedServerGroup.value = '';
  selectedDeviceId.value = '';
  if (transport.value === 'vnt') transport.value = 'hybrid';
}
function saveAccessPassword() {
  cfg.passwordAction = cfg.password ? 'set' : 'clear';
  void saveConfig();
}
const password = ref('');
const passwordVisible = ref(false);
const hostPwVisible = ref(false);
const connectErr = ref('');
const serverErr = ref('');
const serverBusy = ref(false);
const protocol = ref('auto');
const colorDepth = ref(loadColorDepthPref());
watch(colorDepth, saveColorDepthPref);
const resolutionMode = ref(loadResolutionModePref());
const targetResolution = ref(loadTargetResolutionPref());
watch(targetResolution, saveTargetResolutionPref);
const currentScreen = ref<ScreenSize | null>(null);
const targetResolutionOptions = computed(() => resolutionOptions(currentScreen.value));
let screenRefreshGeneration = 0;
async function refreshScreen() {
  const generation = ++screenRefreshGeneration;
  let screen: ScreenSize | null = null;
  try {
    screen = await api.currentScreenSize();
  } catch {
    /* 显示降级选项 */
  }
  if (generation !== screenRefreshGeneration || disposed) return;
  currentScreen.value = screen;
  targetResolution.value = fitResolution(targetResolution.value, screen);
  if (editing.value)
    editing.value.draft.targetResolution = fitResolution(
      editing.value.draft.targetResolution ?? SCREEN_RESOLUTION,
      screen,
    );
}
const showWallpaper = ref(loadShowWallpaperPref());
const dragFullWindows = ref(loadDragFullWindowsPref());
watch(showWallpaper, saveShowWallpaperPref);
watch(dragFullWindows, saveDragFullWindowsPref);
watch(resolutionMode, saveResolutionModePref);
const transport = ref<Transport>('hybrid');
const fecEnabled = ref(false);
const fps = ref(30);
const bitrateKbps = ref(16000);
const connecting = ref(false);
const advOpen = ref(false);
const hostAdvOpen = ref(false);
// 硬件解码偏好：主窗口写 localStorage，控制窗口（同源）创建解码器时读取
const hwDecode = ref(loadHwDecodePref());
function toggleHwDecode() {
  hwDecode.value = !hwDecode.value;
  saveHwDecodePref(hwDecode.value);
}
// 剪贴板同步偏好：控制端新会话的初始同步状态（会话内仍可临时改）
const clipboardSync = ref(loadClipboardSyncPref());
const audioDevices = ref<AudioPlaybackDevice[]>([]);
const audioDevicesLoaded = ref(false);
const audioDevicesLoading = ref(false);
const audioDevicesError = ref('');
const audioPlaybackPreference = ref(loadAudioPlaybackPref());
const audioPlaybackDevice = computed({
  get: () => resolveAudioPlaybackDevice(audioDevices.value, audioPlaybackPreference.value),
  set: (selection: string) => {
    audioPlaybackPreference.value = selection;
    saveAudioPlaybackPref(selection);
  },
});
let audioRefresh: Promise<void> | undefined;
function refreshAudioDevices(): Promise<void> {
  if (audioRefresh) return audioRefresh;
  audioDevicesLoading.value = true;
  audioRefresh = api
    .listAudioPlaybackDevices()
    .then((devices) => {
      if (disposed) return;
      audioDevices.value = devices;
      audioDevicesError.value = '';
    })
    .catch(() => {
      if (!disposed) {
        audioDevices.value = [];
        audioDevicesError.value = '无法获取音频播放设备';
      }
    })
    .finally(() => {
      if (!disposed) {
        audioDevicesLoaded.value = true;
        audioDevicesLoading.value = false;
      }
      audioRefresh = undefined;
    });
  return audioRefresh;
}
function onAudioDevicesFocus() {
  if (!connecting.value) void refreshAudioDevices();
}
function toggleClipboardSync() {
  clipboardSync.value = !clipboardSync.value;
  saveClipboardSyncPref(clipboardSync.value);
}

// 视图切换：远程控制 / 设备列表 / 设置
const view = ref<'main' | 'devices' | 'settings' | 'server'>('main');

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
function winMaximize() {
  tauriWin?.toggleMaximize().catch(() => undefined);
}
function winClose() {
  tauriWin?.close().catch(() => undefined);
}

// 本机局域网地址：展示在服务横幅并可一键复制给对方
const lanIp = ref('');
const copied = ref(false);
async function copyConnection() {
  try {
    await persistConfig.flush();
    const saved = await api.getConfig();
    await navigator.clipboard.writeText(
      formatConnectionShare(`${lanIp.value}:${serverPort.value}`, saved.password),
    );
    serverErr.value = '';
    copied.value = true;
    setTimeout(() => {
      copied.value = false;
    }, 1400);
  } catch (error) {
    serverErr.value = `复制地址和密码失败：${String(error)}`;
  }
}

function pasteConnection(event: ClipboardEvent) {
  try {
    const shared = parseConnectionShare(event.clipboardData?.getData('text/plain') ?? '');
    if (!shared) return;
    event.preventDefault();
    clearServerSelection();
    addr.value = shared.address;
    password.value = shared.password;
    selectedDeviceId.value = '';
    ddOpen.value = false;
    connectErr.value = '';
  } catch (error) {
    event.preventDefault();
    connectErr.value = String(error);
  }
}

// ---------- 已保存设备 ----------
function currentDeviceAdvanced(): DeviceAdvanced {
  return {
    colorDepth: colorDepth.value,
    resolutionMode: resolutionMode.value,
    targetResolution: targetResolution.value,
    showWallpaper: showWallpaper.value,
    dragFullWindows: dragFullWindows.value,
    hwDecode: hwDecode.value,
    clipboardSync: clipboardSync.value,
    audioPlaybackDevice: audioPlaybackPreference.value,
  };
}
const DEVICES_KEY = 'vnt.devices';
const devicesError = ref('');
function loadDevices(): SavedDevice[] {
  try {
    const raw = localStorage.getItem(DEVICES_KEY);
    const list = parseDevices(raw);
    devicesError.value = '';
    return list;
  } catch (error) {
    devicesError.value = String(error);
    return [];
  }
}
const devices = ref<SavedDevice[]>(loadDevices());
function onSharedStorage(event: StorageEvent) {
  if (event.key === DEVICES_KEY || event.key === null) devices.value = loadDevices();
  if (event.key === 'vnt.hwDecode' || event.key === null) hwDecode.value = loadHwDecodePref();
  if (event.key === 'vnt.clipboardSync' || event.key === null)
    clipboardSync.value = loadClipboardSyncPref();
  if (event.key === 'vnt.colorDepth' || event.key === null) colorDepth.value = loadColorDepthPref();
  if (event.key === 'vnt.targetResolution' || event.key === null)
    targetResolution.value = loadTargetResolutionPref();
  if (event.key === 'vnt.resolutionMode' || event.key === null)
    resolutionMode.value = loadResolutionModePref();
  if (event.key === 'vnt.audioPlaybackDevice' || event.key === null)
    audioPlaybackPreference.value = loadAudioPlaybackPref();
}
watch(
  devices,
  (list) => {
    try {
      writeDevices(localStorage, list);
      devicesError.value = '';
    } catch (error) {
      devicesError.value = String(error);
    }
  },
  { deep: true },
);

// 有设备时，连接表单默认应用第一台设备
const selectedDeviceId = ref('');
const liveLabels = ref<{ label: string; deviceId: string }[]>([]);
const liveDeviceIds = computed(() => new Set(liveLabels.value.map((l) => l.deviceId)));
function isLive(id: string): boolean {
  return liveDeviceIds.value.has(id);
}

// 地址下拉：选设备即填充地址密码并应用其保存的连接方式
const ddOpen = ref(false);
const addrWrap = ref<HTMLElement>();
const editing = ref<{ isNew: boolean; draft: SavedDevice & DeviceAdvanced } | null>(null);
const deviceEditorResolution = computed({
  get: () => editing.value?.draft.targetResolution ?? SCREEN_RESOLUTION,
  set: (value: string) => {
    if (editing.value) editing.value.draft.targetResolution = value;
  },
});
const deviceEditorAudio = computed({
  get: () =>
    resolveAudioPlaybackDevice(
      audioDevices.value,
      editing.value?.draft.audioPlaybackDevice ?? null,
    ),
  set: (selection: string) => {
    if (editing.value) editing.value.draft.audioPlaybackDevice = selection;
  },
});
const deviceEditorDialog = ref<HTMLDialogElement>();
watch(editing, async (value) => {
  if (value) {
    value.draft.targetResolution ??= SCREEN_RESOLUTION;
    void refreshScreen();
  }
  await nextTick();
  if (value && !deviceEditorDialog.value?.open) deviceEditorDialog.value?.showModal();
  else if (!value) deviceEditorDialog.value?.close();
});
function closeDeviceEditor() {
  editing.value = null;
}

function formValid(): boolean {
  const d = editing.value?.draft;
  return Boolean(
    d && d.name.trim() && (d.serverNodeId ? connectionIdLabel(d.addr) : parseAddr(d.addr)),
  );
}
function startAdd() {
  void refreshAudioDevices();
  void refreshScreen();
  editing.value = {
    isNew: true,
    draft: {
      ...currentDeviceAdvanced(),
      targetResolution: SCREEN_RESOLUTION,
      id: `dev-${Date.now()}`,
      name: '',
      addr: '',
      password: '',
      transport: 'hybrid',
      fecEnabled: false,
      protocol: 'auto',
      fps: 30,
      bitrateKbps: 16000,
      lastUsed: 0,
    },
  };
}
function startEdit(d: SavedDevice) {
  void refreshAudioDevices();
  void refreshScreen();
  editing.value = { isNew: false, draft: { ...d } };
}
function saveEdit() {
  const e = editing.value;
  if (!e || !formValid()) return;
  const draft = { ...e.draft, name: e.draft.name.trim(), addr: e.draft.addr.trim() };
  devices.value = [draft, ...devices.value.filter((d) => d.id !== draft.id)];
  editing.value = null;
}
function removeDevice(d: SavedDevice) {
  devices.value = devices.value.filter((x) => x.id !== d.id);
  liveLabels.value = liveLabels.value.filter((l) => l.deviceId !== d.id);
  if (selectedDeviceId.value === d.id) selectedDeviceId.value = '';
}
function transportLabel(t: Transport): string {
  return t === 'vnt' ? 'VNT 通道' : t === 'quic' ? 'QUIC' : t === 'tcp' ? '纯 TCP' : '混合';
}
function protocolShort(p: string): string {
  const map: Record<string, string> = {
    auto: '自动协商',
    h264: 'H.264',
    hevc: 'H.265',
    av1: 'AV1',
  };
  return map[p] ?? p;
}
function fmtWhen(ts: number): string {
  if (!ts) return '未连接过';
  const diff = Date.now() - ts;
  if (diff < 60_000) return '刚刚';
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  if (diff < 172_800_000) return '昨天';
  return new Date(ts).toLocaleDateString('zh-CN', { month: 'long', day: 'numeric' });
}
// 把设备参数填进连接表单并标记为当前选中设备；
// 手动修改地址会清除 selectedDeviceId（连接时不再关联该设备）
function applyDevice(d: SavedDevice) {
  selectedServerConfigId.value = d.serverConfigId ?? '';
  selectedServerNodeId.value = d.serverNodeId ?? '';
  selectedServerGroup.value = d.serverNetworkCode ?? '';
  addr.value = d.serverNodeId ? connectionIdLabel(d.addr) || d.addr : d.addr;
  password.value = d.password;
  transport.value = d.transport;
  fecEnabled.value = d.fecEnabled;
  protocol.value = d.protocol;
  fps.value = d.fps;
  bitrateKbps.value = d.bitrateKbps;
  const advanced = d;
  colorDepth.value = advanced.colorDepth;
  resolutionMode.value = advanced.resolutionMode;
  targetResolution.value = advanced.targetResolution ?? SCREEN_RESOLUTION;
  showWallpaper.value = advanced.showWallpaper;
  dragFullWindows.value = advanced.dragFullWindows;
  hwDecode.value = advanced.hwDecode;
  saveHwDecodePref(hwDecode.value);
  clipboardSync.value = advanced.clipboardSync;
  saveClipboardSyncPref(clipboardSync.value);
  audioPlaybackPreference.value = advanced.audioPlaybackDevice;
  saveAudioPlaybackPref(advanced.audioPlaybackDevice);
  selectedDeviceId.value = d.id;
}
function useDevice(d: SavedDevice) {
  applyDevice(d);
  d.lastUsed = Date.now();
  ddOpen.value = false;
}
// 有设备时默认选中第一台（等价于在下拉里选中它：地址、密码、预设一并填入）
if (devices.value.length) applyDevice(devices.value[0]);
// 展示用地址：省略端口时补默认端口，明确实际连接目标
function displayAddr(raw: string): string {
  const parsed = parseAddr(raw);
  return parsed ? formatAddr(parsed.host, parsed.port) : raw;
}

// 角标启动拖动，卡片顺序在松开后保存。
const {
  grid: deviceGrid,
  sortedDevices,
  draggedId,
  start: startDeviceSort,
  cancel: cancelDeviceSort,
} = useDeviceSort(devices);
watch(view, cancelDeviceSort);
function onDocClick(e: MouseEvent) {
  if (ddOpen.value && !addrWrap.value?.contains(e.target as Node)) ddOpen.value = false;
}
function onDocKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') ddOpen.value = false;
}

const unlisteners: UnlistenFn[] = [];
let disposed = false;
let statusRefresh: { changes: Map<number, ServerSessionInfo | null> } | undefined;
function register(unlisten: UnlistenFn) {
  if (disposed) unlisten();
  else unlisteners.push(unlisten);
}

async function refreshMonitors() {
  try {
    const available = await api.listMonitors();
    if (!disposed) monitors.value = available;
  } catch (e) {
    console.error('枚举显示器失败', e);
  }
}

async function refreshStatus() {
  const refresh = { changes: new Map<number, ServerSessionInfo | null>() };
  statusRefresh = refresh;
  try {
    const s = await api.serverStatus();
    if (disposed || statusRefresh !== refresh) return;
    serverRunning.value = s.running;
    serviceInstalled.value = s.installed || edition.value === 'portable';
    serviceState.value = s.state;
    autoStart.value = s.autoStart;
    cfg.passwordConfigured = s.passwordConfigured;
    serviceError.value = s.error ?? '';
    if (s.running && s.monitors.length) monitors.value = s.monitors;
    else if (s.running) void refreshMonitors();
    serverPort.value = s.port;
    const merged = new Map(s.sessions.map((session) => [session.sessionId, session]));
    for (const [id, session] of refresh.changes) {
      if (session) merged.set(id, session);
      else merged.delete(id);
    }
    sessions.value = [...merged.values()];
  } catch (error) {
    serviceState.value = 'fault';
    serviceError.value = String(error);
  } finally {
    if (statusRefresh === refresh) statusRefresh = undefined;
  }
}

async function saveCaptureTarget(patch: Partial<AppConfig>) {
  const previous = {
    selectedTargets: cfg.selectedTargets,
    screenRegions: cfg.screenRegions,
  };
  Object.assign(cfg, patch);
  try {
    await persistConfig();
    configSaveError.value = '';
  } catch (error) {
    Object.assign(cfg, previous);
    configSaveError.value = String(error);
    throw error;
  }
}

async function saveConfig() {
  if (!serviceInstalled.value) return;
  try {
    await persistConfig();
    configSaveError.value = '';
  } catch (error) {
    configSaveError.value = String(error);
  }
}

async function toggleServer() {
  if (serverBusy.value || !serviceInstalled.value) return;
  serverBusy.value = true;
  serverErr.value = '';
  try {
    if (serverRunning.value) {
      await api.serverStop();
      sessions.value = [];
    } else {
      await persistConfig();
      await api.serverStart();
      Object.assign(cfg, await api.getConfig());
    }
  } catch (e) {
    serverErr.value = String(e);
  } finally {
    // 无论成败都刷新真实状态（启动失败时纠正"运行中"显示）
    await refreshStatus();
    serverBusy.value = false;
  }
}

async function toggleAutoStart() {
  if (serverBusy.value || !serviceInstalled.value) return;
  serverBusy.value = true;
  serverErr.value = '';
  const enabling = !autoStart.value;
  try {
    await api.serverSetAutoStart(enabling);
  } catch (error) {
    serverErr.value = String(error);
  } finally {
    await refreshStatus();
    serverBusy.value = false;
  }
}

async function toggleSharedAudio() {
  if (serverBusy.value || !serviceInstalled.value) return;
  cfg.audio = !cfg.audio;
  await saveConfig();
}

const CHARSET = 'abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789';
function genPassword() {
  cfg.passwordAction = 'set';
  const values = crypto.getRandomValues(new Uint8Array(12));
  cfg.password = Array.from(values, (v) => CHARSET[v % CHARSET.length]).join('');
  void saveConfig();
}

async function connect() {
  // Avoid duplicate windows while the current launch request is in flight.
  if (connecting.value) return;
  connectErr.value = '';
  const parsed =
    selectedServerNodeId.value && connectionIdLabel(addr.value)
      ? { host: addr.value, port: DEFAULT_PORT }
      : parseAddr(addr.value);
  if (!parsed) {
    connectErr.value = ADDRESS_ERROR;
    return;
  }
  connecting.value = true;
  try {
    await refreshAudioDevices();
    const supported = await supportedCodecList();
    const codecs =
      protocol.value === 'auto' ? supported : supported.filter((c) => c === protocol.value);
    if (codecs.length === 0) {
      connectErr.value = '当前设备不支持所选视频协议，请选择其他协议或自动协商';
      return;
    }
    if (selectedServerNodeId.value && !selectedServerConfigId.value) {
      throw new Error('请从地址下拉列表重新选择设备所属的服务端配置');
    }
    await refreshScreen();
    const label = await api.openControlWindow({
      clipboardEnabled: clipboardSync.value,
      colorDepth: colorDepth.value,
      showWallpaper: showWallpaper.value,
      dragFullWindows: dragFullWindows.value,
      host: parsed.host,
      port: parsed.port,
      password: password.value,
      codecs,
      hardwareCodecs: await supportedHardwareCodecList(codecs),
      transport: selectedServerNodeId.value ? 'vnt' : transport.value,
      serverConfigId: selectedServerConfigId.value || undefined,
      serverNodeId: selectedServerNodeId.value || undefined,
      serverNetworkCode: selectedServerGroup.value || undefined,
      fecEnabled: (!!selectedServerNodeId.value || transport.value !== 'tcp') && fecEnabled.value,
      ...connectionAudio(audioPlaybackDevice.value),
      fps: fps.value,
      bitrateKbps: bitrateKbps.value,
      resolutionMode: resolutionMode.value,
      ...resolutionTarget(resolutionMode.value, targetResolution.value),
    });
    // 地址未修改过时关联选中的设备：控制窗口存在期间在设备列表显示「远程中」
    if (selectedDeviceId.value && devices.value.some((d) => d.id === selectedDeviceId.value)) {
      liveLabels.value = [...liveLabels.value, { label, deviceId: selectedDeviceId.value }];
    }
  } catch (e) {
    connectErr.value = String(e);
  } finally {
    connecting.value = false;
  }
}

async function refreshPlatform() {
  try {
    platform.value = await api.platformInfo();
  } catch (error) {
    serverErr.value = String(error);
  }
}
async function requestPermission(
  permission: 'screenRecording' | 'accessibility',
  settings = false,
) {
  if (requestingPermission.value) return;
  requestingPermission.value = true;
  try {
    serverErr.value = '';
    await api.requestPlatformPermission(permission, settings);
    await refreshPlatform();
  } catch (error) {
    serverErr.value = String(error);
  } finally {
    requestingPermission.value = false;
  }
}
const appVersion = ref('');
onMounted(async () => {
  void refreshScreen();
  window.addEventListener('focus', refreshScreen);
  register(
    await getCurrentWindow().onMoved(() => {
      void refreshScreen();
    }),
  );
  register(
    await getCurrentWindow().onScaleChanged(() => {
      void refreshScreen();
    }),
  );
  window.addEventListener('storage', onSharedStorage);
  window.addEventListener('focus', refreshPlatform);
  await refreshPlatform();
  try {
    await loadEdition();
    // Portable availability does not depend on the installed service or on
    // successfully reading its configuration during the first status refresh.
    if (edition.value === 'portable') serviceInstalled.value = true;
  } catch (error) {
    serviceError.value = String(error);
  }
  window.addEventListener('focus', onAudioDevicesFocus);
  // Subscribe before loading snapshots or probing codecs, which may be slow.
  // Changes arriving during a snapshot are replayed on top of its result.
  await Promise.all([
    listen<ServerSessionInfo>('server://session', (ev) => {
      statusRefresh?.changes.set(ev.payload.sessionId, ev.payload);
      sessions.value = [
        ...sessions.value.filter((s) => s.sessionId !== ev.payload.sessionId),
        ev.payload,
      ];
    }).then(register),
    listen<{ reason: string; sessionId: number }>('server://session-end', (ev) => {
      statusRefresh?.changes.set(ev.payload.sessionId, null);
      sessions.value = sessions.value.filter((s) => s.sessionId !== ev.payload.sessionId);
    }).then(register),
    // 控制窗口关闭：解除对应设备的「远程中」状态
    listen<string>('remote://closed', (ev) => {
      liveLabels.value = liveLabels.value.filter((l) => l.label !== ev.payload);
    }).then(register),
  ]).catch((error: unknown) => console.error(error));
  if (disposed) return;
  try {
    const saved = await api.getConfig();
    Object.assign(cfg, saved);
  } catch (e) {
    serverErr.value = `读取被控配置失败：${String(e)}`;
  }
  // 兜底：启动时若有设备但表单没填上（存储时序等异常），再应用一次默认设备
  if (!addr.value && devices.value.length) applyDevice(devices.value[0]);
  await Promise.all([refreshMonitors(), refreshStatus(), refreshAudioDevices(), refreshLink()]);
  if (!disposed)
    linkTimer = setInterval(() => {
      void refreshLink();
    }, 3000);
  if (!disposed)
    statusTimer = setInterval(() => {
      void refreshStatus();
    }, 2000);
  codecSupport.value = await probeCodecSupport();
  api
    .localIp()
    .then((ip) => {
      lanIp.value = ip;
    })
    .catch(() => undefined);
  getVersion()
    .then((v) => {
      appVersion.value = v;
    })
    .catch(() => {
      appVersion.value = 'dev';
    });
  document.addEventListener('click', onDocClick);
  document.addEventListener('keydown', onDocKeydown);
});

onUnmounted(() => {
  window.removeEventListener('storage', onSharedStorage);
  window.removeEventListener('focus', refreshPlatform);
  disposed = true;
  clearInterval(statusTimer);
  clearInterval(linkTimer);
  window.removeEventListener('focus', refreshScreen);
  unlisteners.forEach((fn) => fn());
  document.removeEventListener('click', onDocClick);
  document.removeEventListener('keydown', onDocKeydown);
  window.removeEventListener('focus', onAudioDevicesFocus);
});
</script>

<template>
  <svg width="0" height="0" style="position: absolute" aria-hidden="true">
    <defs>
      <symbol id="i-logo" viewBox="0 0 24 24">
        <image href="/icon.svg" width="24" height="24" />
      </symbol>
      <symbol id="i-monitor" viewBox="0 0 24 24">
        <rect x="3" y="4.5" width="18" height="12.5" rx="2" />
        <path d="M8.5 21h7M12 17v4" />
      </symbol>
      <symbol id="i-connect" viewBox="0 0 24 24"><path d="M7 17 17 7M9 7h8v8" /></symbol>
      <symbol id="i-shield" viewBox="0 0 24 24">
        <path d="M12 3.2 19 6v5.2c0 4.3-2.9 7.3-7 8.8-4.1-1.5-7-4.5-7-8.8V6z" />
        <path d="m9.2 11.6 2 2 3.8-4" />
      </symbol>
      <symbol id="i-list" viewBox="0 0 24 24">
        <path d="M9.5 6h10M9.5 12h10M9.5 18h10" />
        <circle cx="4.8" cy="6" r="1.2" fill="currentColor" stroke="none" />
        <circle cx="4.8" cy="12" r="1.2" fill="currentColor" stroke="none" />
        <circle cx="4.8" cy="18" r="1.2" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-sliders" viewBox="0 0 24 24">
        <path d="M4 7h9M17.5 7H20M4 17h5M13.5 17H20M4 12h14" />
        <circle cx="15" cy="7" r="2.1" />
        <circle cx="11" cy="17" r="2.1" />
      </symbol>
      <symbol id="i-chev" viewBox="0 0 24 24"><path d="m6 9.5 6 6 6-6" /></symbol>
      <symbol id="i-copy" viewBox="0 0 24 24">
        <rect x="9" y="9" width="11.5" height="11.5" rx="2" />
        <path
          d="M5.5 14.5h-.4A2.1 2.1 0 0 1 3 12.4V5.1A2.1 2.1 0 0 1 5.1 3h7.3a2.1 2.1 0 0 1 2.1 2.1v.4"
        />
      </symbol>
      <symbol id="i-check" viewBox="0 0 24 24"><path d="m5 12.5 4.6 4.6L19 7.5" /></symbol>
      <symbol id="i-eye" viewBox="0 0 24 24">
        <path d="M2.8 12S6.2 5.8 12 5.8 21.2 12 21.2 12 17.8 18.2 12 18.2 2.8 12 2.8 12Z" />
        <circle cx="12" cy="12" r="2.6" />
      </symbol>
      <symbol id="i-eyeoff" viewBox="0 0 24 24">
        <path
          d="M4 4l16 16M9.9 6.1A9.5 9.5 0 0 1 12 5.8c5.8 0 9.2 6.2 9.2 6.2a17 17 0 0 1-2.7 3.4M6 7.4A16 16 0 0 0 2.8 12S6.2 18.2 12 18.2a9 9 0 0 0 3.9-.9"
        />
      </symbol>
      <symbol id="i-dice" viewBox="0 0 24 24">
        <path d="M20 12a8 8 0 1 1-2.4-5.7M20 4v4.5h-4.5" />
      </symbol>
      <symbol id="i-arrow" viewBox="0 0 24 24"><path d="M4.5 12h15M13.5 6l6 6-6 6" /></symbol>
      <symbol id="i-plus" viewBox="0 0 24 24"><path d="M12 5v14M5 12h14" /></symbol>
      <symbol id="i-edit" viewBox="0 0 24 24">
        <path
          d="m14.5 5.5 4 4M4.5 19.5l1.2-4.6L16.9 3.7a2 2 0 0 1 2.9 0l.5.5a2 2 0 0 1 0 2.9L9.1 18.3l-4.6 1.2Z"
        />
      </symbol>
      <symbol id="i-trash" viewBox="0 0 24 24">
        <path
          d="M4.5 7h15M9.5 7V5.5a1 1 0 0 1 1-1h3a1 1 0 0 1 1 1V7M6.5 7l.9 12.1a1.5 1.5 0 0 0 1.5 1.4h6.2a1.5 1.5 0 0 0 1.5-1.4L17.5 7M10 11v6M14 11v6"
        />
      </symbol>
      <symbol id="i-wifi" viewBox="0 0 24 24">
        <path d="M4.5 11.5a11 11 0 0 1 15 0M7.7 14.8a6.5 6.5 0 0 1 8.6 0" />
        <circle cx="12" cy="18.2" r="1.1" fill="currentColor" stroke="none" />
      </symbol>
      <symbol id="i-min" viewBox="0 0 24 24"><path d="M5.5 12h13" /></symbol>
      <symbol id="i-max" viewBox="0 0 24 24">
        <rect x="6" y="6" width="12" height="12" rx="1.5" />
      </symbol>
      <symbol id="i-x" viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18" /></symbol>
    </defs>
  </svg>

  <div class="app">
    <!-- 标题栏 -->
    <header class="tb">
      <div class="tb-brand" data-tauri-drag-region>
        <svg width="18" height="18" data-tauri-drag-region><use href="#i-logo" /></svg>
        <span class="name" data-tauri-drag-region>VNT Remote</span>
      </div>
      <div class="tb-drag" data-tauri-drag-region></div>
      <div class="tb-ctrl">
        <button type="button" aria-label="最小化" @click="winMinimize">
          <svg class="ic sm"><use href="#i-min" /></svg>
        </button>
        <button type="button" aria-label="最大化/还原" @click="winMaximize">
          <svg class="ic sm"><use href="#i-max" /></svg>
        </button>
        <button
          type="button"
          class="close"
          aria-label="最小化到托盘"
          title="关闭窗口并最小化到托盘"
          @click="winClose"
        >
          <svg class="ic sm"><use href="#i-x" /></svg>
        </button>
      </div>
    </header>

    <div class="hbody">
      <!-- 侧栏 -->
      <aside class="side">
        <div class="nav-cap">工作空间</div>
        <button
          type="button"
          class="nav-item"
          :class="{ active: view === 'main' }"
          @click="view = 'main'"
        >
          <svg class="ic"><use href="#i-monitor" /></svg> 远程控制
        </button>
        <button
          type="button"
          class="nav-item"
          :class="{ active: view === 'devices' }"
          @click="view = 'devices'"
        >
          <svg class="ic"><use href="#i-list" /></svg> 设备列表
          <span class="grow"></span>
          <span v-if="devices.length" class="nav-badge">{{ devices.length }}</span>
        </button>
        <button
          type="button"
          class="nav-item"
          :class="{ active: view === 'settings' }"
          @click="view = 'settings'"
        >
          <svg class="ic"><use href="#i-sliders" /></svg> 设置
        </button>
        <button
          type="button"
          class="nav-item"
          :class="{ active: view === 'server' }"
          @click="view = 'server'"
        >
          <svg class="ic"><use href="#i-connect" /></svg> 服务端
        </button>
        <div class="side-foot">
          <AppUpdater compact />
          <div class="ver">
            VNT Remote v{{ appVersion }} · {{ edition === 'portable' ? '免安装版' : '安装版' }}
          </div>
        </div>
      </aside>

      <main class="main">
        <section v-show="view === 'server'">
          <ServerSettings :statuses="linkStatuses" @saved="linkSaved" />
        </section>
        <!-- 视图：远程控制 -->
        <section v-show="view === 'main'">
          <div class="home-grid">
            <!-- 发起控制 -->
            <section class="card card-pad">
              <div class="card-head">
                <div class="tile t-brand">
                  <svg class="ic"><use href="#i-connect" /></svg>
                </div>
                <div class="grow">
                  <h2>连接远程设备</h2>
                  <div class="sub">输入对方的地址与访问密码，或从下拉中选择已保存设备</div>
                </div>
              </div>

              <div class="fgap"></div>
              <label class="field">
                <span class="lb">设备地址</span>
                <span
                  ref="addrWrap"
                  class="inp has-ic"
                  :class="{ 'has-btn': devices.length > 0 || linkGroups.length > 0 }"
                >
                  <span class="in-ic"
                    ><svg class="ic sm"><use href="#i-monitor" /></svg
                  ></span>
                  <input
                    v-model="addr"
                    placeholder="IP 或域名，可加 :端口，默认 29875"
                    autocomplete="off"
                    spellcheck="false"
                    @input="clearServerSelection"
                    @paste="pasteConnection"
                  />
                  <button
                    v-if="devices.length || linkGroups.length"
                    type="button"
                    class="in-btn"
                    aria-label="从已保存设备中选择"
                    :aria-expanded="ddOpen"
                    aria-haspopup="listbox"
                    @click.stop="ddOpen = !ddOpen"
                  >
                    <svg class="ic sm"><use href="#i-chev" /></svg>
                  </button>
                  <div v-if="ddOpen" class="dd" role="listbox" aria-label="已保存的设备">
                    <div role="group" aria-label="本地设备配置">
                      <span class="dd-head">本地设备配置</span>
                      <button
                        v-for="d in devices"
                        :key="d.id"
                        type="button"
                        class="dd-item"
                        role="option"
                        :aria-selected="selectedDeviceId === d.id"
                        @click="useDevice(d)"
                      >
                        <span class="dn">{{ d.name }}</span
                        ><span class="grow"></span
                        ><span class="da">{{
                          d.serverNodeId ? connectionIdLabel(d.addr) || d.addr : displayAddr(d.addr)
                        }}</span>
                      </button>
                      <span v-if="!devices.length" class="dd-head">暂无本地设备配置</span>
                    </div>
                    <div
                      v-for="group in linkGroups"
                      :key="group.configId"
                      role="group"
                      :aria-label="group.label"
                    >
                      <span class="dd-head">{{ group.label }}</span>
                      <button
                        v-for="device in group.devices"
                        :key="device.id"
                        type="button"
                        class="dd-item"
                        role="option"
                        :aria-selected="
                          !selectedDeviceId &&
                          selectedServerConfigId === group.configId &&
                          selectedServerNodeId === device.id
                        "
                        @click="useOnlineDevice(device, group)"
                      >
                        <span class="dn">{{
                          device.name || connectionIdLabel(device.connectionId)
                        }}</span
                        ><span class="grow"></span
                        ><span class="da">{{ connectionIdLabel(device.connectionId) }}</span>
                      </button>
                      <span v-if="!group.devices.length" class="dd-head">{{
                        group.phase === 'connected'
                          ? '暂无其他在线设备'
                          : linkPhaseLabel(group.phase)
                      }}</span>
                    </div>
                  </div>
                </span>
              </label>
              <div class="fgap"></div>
              <label class="field">
                <span class="lb">访问密码</span>
                <span class="inp">
                  <input
                    v-model="password"
                    :type="passwordVisible ? 'text' : 'password'"
                    placeholder="对方未设置访问密码时可留空"
                    autocomplete="off"
                  />
                  <button
                    type="button"
                    class="in-btn"
                    aria-label="显示/隐藏密码"
                    @click="passwordVisible = !passwordVisible"
                  >
                    <svg class="ic sm">
                      <use :href="passwordVisible ? '#i-eyeoff' : '#i-eye'" />
                    </svg>
                  </button>
                </span>
              </label>
              <div class="fgap"></div>
              <button
                class="btn btn-primary btn-lg btn-block"
                type="submit"
                :disabled="connecting"
                @click="connect"
              >
                <svg class="ic"><use href="#i-arrow" /></svg>
                {{ connecting ? '正在打开窗口…' : '连接' }}
              </button>
              <p v-if="devicesError" class="error-text" role="alert">{{ devicesError }}</p>
              <p v-if="connectErr" class="error-text" role="alert">{{ connectErr }}</p>

              <!-- 高级选项 -->
              <div class="adv" :class="{ open: advOpen }">
                <div
                  class="divider"
                  role="button"
                  tabindex="0"
                  :aria-expanded="advOpen"
                  @click="advOpen = !advOpen"
                  @keydown.enter.prevent="advOpen = !advOpen"
                  @keydown.space.prevent="advOpen = !advOpen"
                >
                  <span
                    >高级选项
                    <svg class="ic sm chev" style="vertical-align: -2px"><use href="#i-chev" /></svg
                  ></span>
                </div>
                <div v-show="advOpen" class="adv-body">
                  <div class="seg-row">
                    <span class="slb">通信协议</span>
                    <span v-if="selectedServerNodeId" class="tag">VNT 通道</span>
                    <div v-else class="seg" role="group" aria-label="通信协议">
                      <button
                        type="button"
                        :class="{ on: transport === 'hybrid' }"
                        @click="transport = 'hybrid'"
                      >
                        混合 TCP + UDP
                      </button>
                      <button
                        type="button"
                        :class="{ on: transport === 'tcp' }"
                        @click="transport = 'tcp'"
                      >
                        纯 TCP
                      </button>
                      <button
                        type="button"
                        :class="{ on: transport === 'quic' }"
                        @click="transport = 'quic'"
                      >
                        QUIC
                      </button>
                    </div>
                  </div>
                  <div class="seg-row">
                    <span class="slb">自适应 FEC</span>
                    <button
                      type="button"
                      class="sw"
                      :class="{ on: fecEnabled }"
                      role="switch"
                      :aria-checked="fecEnabled"
                      aria-label="自适应 FEC"
                      :disabled="!selectedServerNodeId && transport === 'tcp'"
                      @click="fecEnabled = !fecEnabled"
                    ></button>
                    <span class="codec-cap">{{
                      !selectedServerNodeId && transport === 'tcp'
                        ? '适用于混合模式、QUIC 和 VNT'
                        : '丢包时稳定画面'
                    }}</span>
                  </div>
                  <div class="seg-row">
                    <span class="slb">视频编码</span>
                    <div class="seg" role="group" aria-label="视频编码协议">
                      <button
                        type="button"
                        :class="{ on: protocol === 'auto' }"
                        @click="protocol = 'auto'"
                      >
                        自动协商
                      </button>
                      <button
                        type="button"
                        :class="{ on: protocol === 'h264' }"
                        :disabled="!codecSupport.h264"
                        @click="protocol = 'h264'"
                      >
                        H.264
                      </button>
                      <button
                        type="button"
                        :class="{ on: protocol === 'hevc' }"
                        :disabled="!codecSupport.hevc"
                        @click="protocol = 'hevc'"
                      >
                        H.265
                      </button>
                      <button
                        type="button"
                        :class="{ on: protocol === 'av1' }"
                        :disabled="!codecSupport.av1"
                        @click="protocol = 'av1'"
                      >
                        AV1
                      </button>
                    </div>
                  </div>
                  <div class="seg-row">
                    <span class="slb">目标帧率</span>
                    <div class="seg" role="group" aria-label="目标帧率">
                      <button
                        v-for="f in [30, 60]"
                        :key="f"
                        type="button"
                        :class="{ on: fps === f }"
                        @click="fps = f"
                      >
                        {{ f }}
                      </button>
                    </div>
                    <span class="codec-cap">fps</span>
                  </div>
                  <div class="seg-row">
                    <span class="slb">目标码率</span>
                    <div class="seg" role="group" aria-label="目标码率">
                      <button
                        v-for="b in [8000, 16000, 30000, 50000]"
                        :key="b"
                        type="button"
                        :class="{ on: bitrateKbps === b }"
                        @click="bitrateKbps = b"
                      >
                        {{ b / 1000 }}
                      </button>
                    </div>
                    <span class="codec-cap">Mbps</span>
                  </div>
                  <div class="seg-row">
                    <label class="slb" for="color-depth">颜色深度</label>
                    <span class="inp color-depth-select">
                      <AppSelect
                        id="color-depth"
                        v-model="colorDepth"
                        aria-label="颜色深度"
                        aria-describedby="color-depth-hint"
                        :options="
                          [15, 16, 24, 32].map((value) => ({ value, label: `${value} 位` }))
                        "
                      />
                    </span>
                    <span id="color-depth-hint" class="codec-cap">越低越流畅</span>
                  </div>
                  <div class="seg-row resolution-row">
                    <label class="slb" for="resolution-mode">分辨率模式</label>
                    <span class="inp">
                      <AppSelect
                        id="resolution-mode"
                        v-model="resolutionMode"
                        aria-label="分辨率模式"
                        aria-describedby="resolution-hint"
                        :options="[
                          { value: 'none', label: '不处理分辨率' },
                          { value: 'scale', label: '缩放分辨率' },
                          { value: 'change', label: '修改被控分辨率' },
                        ]"
                      />
                    </span>
                    <span id="resolution-hint" class="codec-cap">{{
                      resolutionMode === 'none'
                        ? '按被控原始分辨率传输'
                        : resolutionMode === 'scale'
                          ? '软件等比缩小画面，适配所选分辨率'
                          : '修改被控屏幕分辨率，断开后恢复；不可用时回退软件缩放'
                    }}</span>
                  </div>
                  <div v-if="resolutionMode !== 'none'" class="seg-row">
                    <label class="slb" for="target-resolution">分辨率</label>
                    <span class="inp">
                      <AppSelect
                        id="target-resolution"
                        v-model="targetResolution"
                        aria-label="分辨率"
                        :options="targetResolutionOptions"
                        :disabled="!currentScreen"
                        @open="refreshScreen"
                      />
                    </span>
                  </div>
                  <div class="seg-row">
                    <span class="slb">桌面背景</span>
                    <button
                      type="button"
                      class="sw"
                      :class="{ on: showWallpaper }"
                      role="switch"
                      :aria-checked="showWallpaper"
                      aria-label="桌面背景"
                      @click="showWallpaper = !showWallpaper"
                    ></button>
                    <span class="codec-cap">关闭时隐藏被控桌面背景</span>
                  </div>
                  <div class="seg-row">
                    <span class="slb">拖动时显示窗口内容</span>
                    <button
                      type="button"
                      class="sw"
                      :class="{ on: dragFullWindows }"
                      role="switch"
                      :aria-checked="dragFullWindows"
                      aria-label="拖动时显示窗口内容"
                      @click="dragFullWindows = !dragFullWindows"
                    ></button>
                    <span class="codec-cap">关闭时仅显示窗口边框</span>
                  </div>
                  <div class="seg-row">
                    <span class="slb">硬件解码</span>
                    <button
                      type="button"
                      class="sw"
                      :class="{ on: hwDecode }"
                      role="switch"
                      :aria-checked="hwDecode"
                      aria-label="硬件解码"
                      @click="toggleHwDecode"
                    ></button>
                    <span class="codec-cap">优先显卡解码，不可用时自动回退软件解码</span>
                  </div>
                  <div class="seg-row audio-playback-row">
                    <label class="slb" for="audio-playback">音频播放</label>
                    <span class="inp">
                      <AppSelect
                        id="audio-playback"
                        v-model="audioPlaybackDevice"
                        aria-label="音频播放"
                        aria-describedby="audio-playback-hint"
                        :options="[
                          ...audioDevices.map((device) => ({
                            value: device.id,
                            label: device.name + (device.isDefault ? '（默认）' : ''),
                          })),
                          { value: AUDIO_PLAYBACK_DISABLED, label: '不同步音频' },
                        ]"
                        :disabled="connecting || (!audioDevicesLoaded && audioDevicesLoading)"
                        @open="onAudioDevicesFocus"
                      />
                    </span>
                    <span id="audio-playback-hint" class="codec-cap" role="status">{{
                      audioDevicesError ||
                      (audioDevicesLoaded && !audioDevices.length
                        ? '暂无可播放音频的设备'
                        : '同步远端设备的音频')
                    }}</span>
                  </div>
                  <div class="seg-row">
                    <span class="slb">剪贴板同步</span>
                    <button
                      type="button"
                      class="sw"
                      :class="{ on: clipboardSync }"
                      role="switch"
                      :aria-checked="clipboardSync"
                      aria-label="剪贴板同步"
                      @click="toggleClipboardSync"
                    ></button>
                    <span class="codec-cap">与远程设备互相同步剪贴板</span>
                  </div>
                </div>
              </div>
            </section>

            <!-- 被控访问 -->
            <div class="col">
              <section class="card card-pad">
                <div class="card-head">
                  <div class="tile t-ok">
                    <svg class="ic"><use href="#i-shield" /></svg>
                  </div>
                  <div class="grow">
                    <h2>允许访问本机</h2>
                  </div>
                  <button
                    type="button"
                    class="sw"
                    :class="{ on: serverRunning }"
                    role="switch"
                    :aria-checked="serverRunning"
                    :aria-label="serverRunning ? '停止本机服务' : '启动本机服务'"
                    :disabled="
                      serverBusy ||
                      !serviceInstalled ||
                      serviceState === 'starting' ||
                      serviceState === 'stopping'
                    "
                    @click="toggleServer"
                  ></button>
                </div>

                <div v-if="platform?.platform === 'macos'" class="mac-permissions">
                  <p>
                    macOS 免安装版 · 支持文本和图片剪贴板，暂不支持文件复制粘贴；锁屏时暂停共享。
                  </p>
                  <div v-if="!platform.screenRecording">
                    屏幕录制权限未开启
                    <button
                      type="button"
                      class="btn"
                      :disabled="requestingPermission"
                      @click="requestPermission('screenRecording')"
                    >
                      {{ requestingPermission ? '等待授权…' : '请求授权' }}
                    </button>
                    <button
                      type="button"
                      class="btn"
                      :disabled="requestingPermission"
                      @click="requestPermission('screenRecording', true)"
                    >
                      打开系统设置
                    </button>
                  </div>
                  <div v-if="!platform.accessibility">
                    辅助功能权限未开启；关闭「允许控制」可仅共享画面
                    <button
                      type="button"
                      class="btn"
                      :disabled="requestingPermission"
                      @click="requestPermission('accessibility')"
                    >
                      {{ requestingPermission ? '等待授权…' : '请求授权' }}
                    </button>
                    <button
                      type="button"
                      class="btn"
                      :disabled="requestingPermission"
                      @click="requestPermission('accessibility', true)"
                    >
                      打开系统设置
                    </button>
                  </div>
                  <p v-if="!platform.screenRecording">授权后可能需要退出并重新打开应用。</p>
                </div>
                <div class="fgap"></div>
                <div class="svc-banner" :class="serverRunning ? 'on' : 'off'">
                  <span class="dot" :class="{ live: serverRunning }"></span>
                  <span class="grow num">
                    {{
                      serverRunning
                        ? lanIp
                          ? `服务运行中 · ${lanIp}:${serverPort}`
                          : `服务运行中 · 端口 ${serverPort}`
                        : serviceInstalled
                          ? '服务已停止 — 打开开关后其他设备才能连接本机'
                          : '请先安装程序'
                    }}
                  </span>
                  <button
                    v-if="serverRunning && lanIp"
                    type="button"
                    class="icon-btn"
                    :aria-label="copied ? '已复制' : '复制地址和密码'"
                    :title="copied ? '已复制' : '复制地址和密码'"
                    @click="copyConnection"
                  >
                    <svg class="ic sm"><use :href="copied ? '#i-check' : '#i-copy'" /></svg>
                  </button>
                </div>
                <p
                  v-if="serverErr || configSaveError || serviceError"
                  class="error-text"
                  role="alert"
                >
                  {{ serverErr || configSaveError || serviceError }}
                </p>

                <p class="sd">被控边框设置实时生效；其他配置除端口外，对新会话生效。</p>

                <label class="field">
                  <span class="lb">本机访问密码</span>
                  <span class="inp has-btn2">
                    <input
                      v-model="cfg.password"
                      :type="hostPwVisible ? 'text' : 'password'"
                      placeholder="留空允许免密访问"
                      autocomplete="off"
                      :disabled="serverBusy || !serviceInstalled"
                      @change="saveAccessPassword"
                    />
                    <button
                      type="button"
                      class="in-btn"
                      aria-label="显示/隐藏密码"
                      @click="hostPwVisible = !hostPwVisible"
                    >
                      <svg class="ic sm">
                        <use :href="hostPwVisible ? '#i-eyeoff' : '#i-eye'" />
                      </svg>
                    </button>
                    <button
                      type="button"
                      class="in-btn"
                      aria-label="生成随机密码"
                      title="生成随机密码"
                      :disabled="serverBusy || !serviceInstalled"
                      @click="genPassword"
                    >
                      <svg class="ic sm"><use href="#i-dice" /></svg>
                    </button>
                  </span>
                </label>
                <div class="fgap"></div>
                <div class="frow">
                  <label class="field" style="width: 120px">
                    <span class="lb">服务端口</span>
                    <span class="inp"
                      ><input
                        v-model.number="cfg.port"
                        type="number"
                        required
                        min="1"
                        max="65535"
                        :disabled="serverBusy || portLocked || !serviceInstalled"
                        :title="portLocked ? '请先停止被控服务再修改端口' : undefined"
                        @change="saveConfig"
                    /></span>
                  </label>
                  <ScreenRegions
                    :config="cfg"
                    :monitors="monitors"
                    :disabled="serverBusy || !serviceInstalled"
                    :save="saveCaptureTarget"
                    @refresh="refreshMonitors"
                  />
                </div>

                <div class="adv" :class="{ open: hostAdvOpen }">
                  <div
                    class="divider"
                    role="button"
                    tabindex="0"
                    :aria-expanded="hostAdvOpen"
                    @click="hostAdvOpen = !hostAdvOpen"
                    @keydown.enter.prevent="hostAdvOpen = !hostAdvOpen"
                    @keydown.space.prevent="hostAdvOpen = !hostAdvOpen"
                  >
                    <span
                      >高级选项
                      <svg class="ic sm chev" style="vertical-align: -2px">
                        <use href="#i-chev" /></svg
                    ></span>
                  </div>
                  <div v-show="hostAdvOpen" class="adv-body">
                    <div class="srow" style="margin-top: 6px">
                      <div class="grow">
                        <div class="st">剪贴板同步</div>
                        <div class="sd">
                          {{
                            platform?.fileClipboard === false
                              ? '在两台设备之间复制粘贴文本与图片'
                              : '在两台设备之间复制粘贴文本、图片与文件'
                          }}
                        </div>
                      </div>
                      <button
                        type="button"
                        class="sw"
                        :class="{ on: cfg.clipboard }"
                        role="switch"
                        :aria-checked="cfg.clipboard"
                        aria-label="剪贴板同步"
                        :disabled="serverBusy || !serviceInstalled"
                        @click="
                          cfg.clipboard = !cfg.clipboard;
                          saveConfig();
                        "
                      ></button>
                    </div>

                    <div class="srow" style="margin-top: 6px">
                      <div class="grow">
                        <div class="st">共享系统音频</div>
                        <div class="sd">共享播放音频，不包含麦克风</div>
                      </div>
                      <button
                        type="button"
                        class="sw"
                        :class="{ on: cfg.audio }"
                        role="switch"
                        :aria-checked="cfg.audio"
                        aria-label="共享系统音频"
                        :disabled="serverBusy || !serviceInstalled"
                        @click="toggleSharedAudio"
                      ></button>
                    </div>

                    <div class="srow" style="margin-top: 6px">
                      <div class="grow">
                        <div class="st">允许控制</div>
                        <div class="sd">允许主控端操作本机</div>
                      </div>
                      <button
                        type="button"
                        class="sw"
                        :class="{ on: cfg.allowControl }"
                        role="switch"
                        :aria-checked="cfg.allowControl"
                        aria-label="允许控制"
                        :disabled="serverBusy || !serviceInstalled"
                        @click="
                          cfg.allowControl = !cfg.allowControl;
                          saveConfig();
                        "
                      ></button>
                    </div>

                    <div class="srow" style="margin-top: 6px">
                      <div class="grow">
                        <div class="st">硬件编码</div>
                        <div class="sd">使用显卡压缩视频画面</div>
                      </div>
                      <button
                        type="button"
                        class="sw"
                        :class="{ on: cfg.hardwareEncode }"
                        role="switch"
                        :aria-checked="cfg.hardwareEncode"
                        aria-label="硬件编码"
                        :disabled="serverBusy || !serviceInstalled"
                        @click="
                          cfg.hardwareEncode = !cfg.hardwareEncode;
                          saveConfig();
                        "
                      ></button>
                    </div>

                    <div v-if="platform?.platform !== 'macos'" class="srow" style="margin-top: 6px">
                      <div class="grow">
                        <div class="st">显示被控边框</div>
                        <div class="sd">在被共享的屏幕周围显示提示边框，修改后立即生效</div>
                      </div>
                      <button
                        type="button"
                        class="sw"
                        :class="{ on: cfg.captureBorder }"
                        role="switch"
                        :aria-checked="cfg.captureBorder"
                        aria-label="显示被控边框"
                        :disabled="serverBusy || !serviceInstalled"
                        @click="
                          cfg.captureBorder = !cfg.captureBorder;
                          saveConfig();
                        "
                      ></button>
                    </div>
                  </div>
                </div>

                <div v-if="sessions.length" class="sess">
                  <div class="sess-h">
                    <svg class="ic sm"><use href="#i-monitor" /></svg> 已连接设备
                    <span class="grow"></span>
                    <span class="tag ok">{{ sessions.length }} 台在线</span>
                  </div>
                  <div v-for="session in sessions" :key="session.sessionId" class="sess-item">
                    <div class="tile t-ok" style="width: 30px; height: 30px; border-radius: 9px">
                      <svg class="ic sm"><use href="#i-monitor" /></svg>
                    </div>
                    <div class="grow">
                      <div class="t num">{{ session.peer }}</div>
                      <div class="s">
                        {{ session.codec.toUpperCase() }} ·
                        {{
                          session.transport === 'vnt'
                            ? 'VNT 通道'
                            : session.transport === 'quic'
                              ? 'QUIC'
                              : session.transport === 'tcp'
                                ? '纯 TCP'
                                : '混合模式'
                        }}
                      </div>
                    </div>
                    <span v-if="session.viewOnly" class="tag">观看</span>
                  </div>
                </div>
              </section>
            </div>
          </div>
        </section>

        <!-- 视图：设备列表 -->
        <section v-show="view === 'devices'">
          <div class="page-head">
            <div>
              <h1>设备列表</h1>
              <p>保存常用设备的地址与连接方式，一键发起连接。</p>
            </div>
            <div class="grow"></div>
            <button type="button" class="btn btn-primary" :disabled="!!editing" @click="startAdd">
              <svg class="ic sm"><use href="#i-plus" /></svg> 添加设备
            </button>
          </div>

          <div ref="deviceGrid" class="dev-grid">
            <TransitionGroup name="dev-sort">
              <div
                v-for="d in sortedDevices"
                :key="d.id"
                class="card dev-card"
                :class="{ 'dev-card-placeholder': draggedId === d.id }"
                :data-device-id="d.id"
              >
                <div class="dev-head">
                  <div class="tile t-neutral">
                    <svg class="ic sm"><use href="#i-monitor" /></svg>
                  </div>
                  <div class="grow">
                    <div class="dev-name">{{ d.name }}</div>
                    <div class="dev-addr num">
                      {{
                        d.serverNodeId ? connectionIdLabel(d.addr) || d.addr : displayAddr(d.addr)
                      }}
                    </div>
                  </div>
                  <span v-if="isLive(d.id)" class="tag ok">远程中</span>
                  <span class="dev-time num">{{ fmtWhen(d.lastUsed) }}</span>
                </div>
                <div class="dev-tags">
                  <span class="tag">{{ transportLabel(d.transport) }}</span>
                  <span class="tag">{{ protocolShort(d.protocol) }}</span>
                  <span class="tag num">{{ d.fps }} fps</span>
                  <span class="tag num">{{ d.bitrateKbps / 1000 }} Mbps</span>
                </div>
                <div class="dev-foot">
                  <button
                    type="button"
                    class="btn btn-primary btn-sm"
                    @click="
                      useDevice(d);
                      view = 'main';
                      connect();
                    "
                  >
                    <svg class="ic sm"><use href="#i-arrow" /></svg> 连接
                  </button>
                  <span class="grow"></span>
                  <button
                    type="button"
                    class="icon-btn"
                    aria-label="编辑连接方式"
                    title="编辑连接方式"
                    @click="startEdit(d)"
                  >
                    <svg class="ic sm"><use href="#i-edit" /></svg>
                  </button>
                  <button
                    type="button"
                    class="icon-btn"
                    aria-label="删除设备"
                    title="删除设备"
                    @click="removeDevice(d)"
                  >
                    <svg class="ic sm"><use href="#i-trash" /></svg>
                  </button>
                </div>
                <button
                  type="button"
                  class="dev-drag"
                  aria-label="拖动排序"
                  title="拖动排序"
                  @pointerdown="startDeviceSort(d.id, $event)"
                  @dragstart.prevent
                  @click.stop
                >
                  <svg viewBox="0 0 16 16" aria-hidden="true">
                    <path d="M5 12 12 5M9 12l3-3" />
                  </svg>
                </button>
              </div>

              <button
                :disabled="!!editing"
                key="device-add"
                type="button"
                class="dev-add"
                @click="startAdd"
              >
                <svg class="ic" style="width: 20px; height: 20px"><use href="#i-plus" /></svg>
                添加设备
                <small>保存地址与连接方式</small>
              </button>
            </TransitionGroup>
          </div>
        </section>

        <!-- 视图：设置 -->
        <section v-show="view === 'settings'">
          <div class="page-head">
            <div>
              <h1>设置</h1>
              <p>应用外观与被控服务默认值。</p>
            </div>
          </div>
          <div class="settings-grid">
            <div class="scards">
              <section class="card">
                <div class="card-pad">
                  <div class="card-head" style="padding-bottom: 6px">
                    <div class="tile t-brand">
                      <svg class="ic"><use href="#i-sliders" /></svg>
                    </div>
                    <h2>通用</h2>
                  </div>
                  <div v-if="edition === 'installed'" class="srow">
                    <div class="grow">
                      <div class="st">Windows 服务开机启动</div>
                      <div class="sd">
                        开机启动 Windows
                        服务，登录后自动打开桌面端；被控按上次开关恢复。此开关不改变当前被控状态。
                      </div>
                    </div>
                    <button
                      class="sw"
                      type="button"
                      role="switch"
                      :class="{ on: autoStart }"
                      :aria-checked="autoStart"
                      aria-label="Windows 服务开机启动"
                      :disabled="serverBusy || !serviceInstalled"
                      @click="toggleAutoStart"
                    ></button>
                  </div>
                  <div class="srow">
                    <div class="grow">
                      <div class="st">
                        {{ edition === 'portable' ? '免安装版被控' : 'Windows 被控服务' }} ·
                        {{ serviceLabel }}
                      </div>
                      <div class="sd">
                        {{
                          edition === 'portable'
                            ? '由当前进程提供被控，关闭窗口后驻留托盘，退出程序即停止。锁屏或 UAC 时暂停，返回普通桌面后恢复。不支持开机自启或 Ctrl+Alt+Del。被控配置保存在当前用户应用数据目录。'
                            : serviceInstalled
                              ? '关闭主窗口后驻留托盘；退出程序会停止 Windows 服务。关闭被控保留服务，被控启停无需再次提权；启动服务和设置开机启动需要管理员授权。配置通过本地 TCP 由 Windows 服务统一读写，桌面不直接访问配置文件。'
                              : '请先安装程序'
                        }}
                      </div>
                      <p
                        v-if="serverErr || configSaveError || serviceError"
                        class="error-text"
                        role="alert"
                      >
                        {{ serverErr || configSaveError || serviceError }}
                      </p>
                    </div>
                  </div>
                  <div class="srow">
                    <div class="grow">
                      <div class="st">主题</div>
                      <div class="sd">主界面跟随此设置；远程控制窗口始终保持深色</div>
                    </div>
                    <div class="seg" role="group" aria-label="主题">
                      <button
                        v-for="m in ['light', 'dark', 'system'] as ThemeMode[]"
                        :key="m"
                        type="button"
                        :class="{ on: themeMode === m }"
                        @click="setThemeMode(m)"
                      >
                        {{ m === 'light' ? '浅色' : m === 'dark' ? '深色' : '跟随系统' }}
                      </button>
                    </div>
                  </div>
                  <div class="srow">
                    <div class="grow">
                      <div class="st">屏幕区域</div>
                      <ScreenRegions
                        mode="management"
                        :config="cfg"
                        :monitors="monitors"
                        :disabled="serverBusy || !serviceInstalled"
                        :save="saveCaptureTarget"
                        @refresh="refreshMonitors"
                      />
                    </div>
                  </div>
                </div>
              </section>
            </div>
            <div class="scards">
              <section class="card">
                <div class="card-pad">
                  <div class="card-head" style="padding-bottom: 10px">
                    <svg width="34" height="34"><use href="#i-logo" /></svg>
                    <div class="grow">
                      <h2>VNT Remote</h2>
                      <div class="sub num">v{{ appVersion }}</div>
                    </div>
                  </div>
                  <div class="srow">
                    <div class="grow">
                      <div class="st">关于本应用</div>
                      <div class="sd">局域网远程桌面</div>
                    </div>
                  </div>
                  <div class="srow">
                    <div class="grow"><AppUpdater /></div>
                  </div>
                </div>
              </section>
            </div>
          </div>
        </section>
      </main>
    </div>
  </div>
  <Teleport to="body">
    <dialog
      ref="deviceEditorDialog"
      class="profile-dialog"
      aria-labelledby="device-editor-title"
      @cancel.prevent="closeDeviceEditor"
      @click="$event.target === deviceEditorDialog && closeDeviceEditor()"
    >
      <form v-if="editing" @submit.prevent="saveEdit">
        <div class="editor-header">
          <h2 id="device-editor-title">{{ editing.isNew ? '添加设备' : '编辑设备' }}</h2>
          <button type="button" class="icon-btn" aria-label="关闭" @click="closeDeviceEditor">
            <svg class="ic sm"><use href="#i-x" /></svg>
          </button>
        </div>
        <div class="editor-body">
          <div class="editor-grid">
            <label class="field">
              <span class="lb">名称</span>
              <span class="inp"
                ><input v-model="editing.draft.name" placeholder="例如 客厅主机" autofocus
              /></span>
            </label>
            <label class="field">
              <span class="lb">{{ editing.draft.serverNodeId ? '连接 ID' : '地址' }}</span>
              <span class="inp"
                ><input
                  v-if="editing.draft.serverNodeId"
                  :value="connectionIdLabel(editing.draft.addr)"
                  readonly
                  class="num" /><input
                  v-else
                  v-model="editing.draft.addr"
                  class="num"
                  placeholder="IP 或域名，可加 :端口，默认 29875"
              /></span>
              <span
                v-if="
                  !editing.draft.serverNodeId &&
                  editing.draft.addr &&
                  !parseAddr(editing.draft.addr)
                "
                class="error-text"
                role="alert"
                >{{ ADDRESS_ERROR }}</span
              >
            </label>
            <label class="field">
              <span class="lb">访问密码</span>
              <span class="inp"
                ><input
                  v-model="editing.draft.password"
                  type="password"
                  placeholder="可留空，连接时填写"
                  autocomplete="off"
              /></span>
            </label>
            <label class="field">
              <span class="lb">通信协议</span>
              <span class="inp">
                <span v-if="editing.draft.serverNodeId" class="tag"
                  >VNT 通道 · {{ editing.draft.serverNetworkCode }} /
                  {{ connectionIdLabel(editing.draft.addr) }}</span
                >
                <AppSelect
                  v-else
                  v-model="editing.draft.transport"
                  aria-label="通信协议"
                  :options="[
                    { value: 'hybrid', label: '混合 TCP + UDP' },
                    { value: 'tcp', label: '纯 TCP' },
                    { value: 'quic', label: 'QUIC' },
                  ]"
                />
              </span>
            </label>
            <div class="field">
              <span class="lb">自适应 FEC</span>
              <div class="seg-row">
                <button
                  type="button"
                  class="sw"
                  :class="{ on: editing.draft.fecEnabled }"
                  role="switch"
                  :aria-checked="!!editing.draft.fecEnabled"
                  aria-label="自适应 FEC"
                  :disabled="editing.draft.transport === 'tcp'"
                  @click="editing.draft.fecEnabled = !editing.draft.fecEnabled"
                ></button>
                <span class="codec-cap">{{
                  editing.draft.transport === 'tcp'
                    ? '适用于混合模式、QUIC 和 VNT'
                    : '丢包时稳定画面'
                }}</span>
              </div>
            </div>
            <label class="field">
              <span class="lb">视频编码</span>
              <span class="inp">
                <AppSelect
                  v-model="editing.draft.protocol"
                  aria-label="视频编码"
                  :options="[
                    { value: 'auto', label: '自动协商' },
                    { value: 'h264', label: 'H.264' },
                    { value: 'hevc', label: 'H.265' },
                    { value: 'av1', label: 'AV1' },
                  ]"
                />
              </span>
            </label>
            <label class="field">
              <span class="lb">帧率 / 码率</span>
              <span class="inp combo">
                <AppSelect
                  v-model="editing.draft.fps"
                  aria-label="目标帧率"
                  :options="[30, 60].map((value) => ({ value, label: `${value} fps` }))"
                />
                <AppSelect
                  v-model="editing.draft.bitrateKbps"
                  aria-label="目标码率"
                  :options="
                    [8000, 16000, 30000, 50000].map((value) => ({
                      value,
                      label: `${value / 1000} Mbps`,
                    }))
                  "
                />
              </span>
            </label>
            <label class="field">
              <span class="lb">颜色深度</span>
              <AppSelect
                v-model="editing.draft.colorDepth"
                aria-label="颜色深度"
                :options="[15, 16, 24, 32].map((value) => ({ value, label: `${value} 位` }))"
              />
            </label>
            <label class="field">
              <span class="lb">分辨率模式</span>
              <AppSelect
                v-model="editing.draft.resolutionMode"
                aria-label="分辨率模式"
                :options="[
                  { value: 'none', label: '不处理分辨率' },
                  { value: 'scale', label: '缩放分辨率' },
                  { value: 'change', label: '修改被控分辨率' },
                ]"
              />
            </label>
            <label v-if="editing.draft.resolutionMode !== 'none'" class="field">
              <span class="lb">分辨率</span>
              <AppSelect
                v-model="deviceEditorResolution"
                aria-label="分辨率"
                :options="targetResolutionOptions"
                :disabled="!currentScreen"
                @open="refreshScreen"
              />
            </label>
            <label class="field device-audio-field">
              <span class="lb">音频播放</span>
              <AppSelect
                v-model="deviceEditorAudio"
                aria-label="音频播放"
                :options="[
                  ...audioDevices.map((device) => ({
                    value: device.id,
                    label: device.name + (device.isDefault ? '（默认）' : ''),
                  })),
                  { value: AUDIO_PLAYBACK_DISABLED, label: '不同步音频' },
                ]"
                :disabled="!audioDevicesLoaded && audioDevicesLoading"
                @open="onAudioDevicesFocus"
              />
              <span v-if="audioDevicesError" class="codec-cap" role="status">{{
                audioDevicesError
              }}</span>
            </label>
            <div
              v-for="option in [
                { key: 'showWallpaper', label: '桌面背景', hint: '关闭时隐藏被控桌面背景' },
                {
                  key: 'dragFullWindows',
                  label: '拖动时显示窗口内容',
                  hint: '关闭时仅显示窗口边框',
                },
                {
                  key: 'hwDecode',
                  label: '硬件解码',
                  hint: '优先显卡解码，不可用时自动回退软件解码',
                },
                { key: 'clipboardSync', label: '剪贴板同步', hint: '与远程设备互相同步剪贴板' },
              ] as const"
              :key="option.key"
              class="field"
            >
              <span class="lb">{{ option.label }}</span>
              <div class="seg-row">
                <button
                  type="button"
                  class="sw"
                  :class="{ on: editing.draft[option.key] }"
                  role="switch"
                  :aria-checked="editing.draft[option.key]"
                  :aria-label="option.label"
                  @click="editing.draft[option.key] = !editing.draft[option.key]"
                ></button>
                <span class="codec-cap">{{ option.hint }}</span>
              </div>
            </div>
          </div>
        </div>
        <div class="editor-footer">
          <button type="button" class="btn btn-secondary btn-sm" @click="closeDeviceEditor">
            取消
          </button>
          <button type="submit" class="btn btn-primary btn-sm" :disabled="!formValid()">
            保存配置
          </button>
        </div>
      </form>
    </dialog>
  </Teleport>
</template>

<style scoped>
.device-audio-field {
  grid-column: 1 / -1;
}

.mac-permissions {
  margin-top: 12px;
  font-size: 12px;
  line-height: 1.7;
}
.mac-permissions .btn {
  margin: 4px;
}
.color-depth-select {
  flex: 0 0 110px;
}
.resolution-row {
  display: grid;
  grid-template-columns: 64px minmax(0, 360px);
  gap: 8px 12px;
}
#resolution-hint {
  grid-column: 2;
}
.audio-playback-row {
  display: grid;
  grid-template-columns: 64px minmax(0, 360px);
  gap: 8px 12px;
}
#audio-playback-hint {
  grid-column: 2;
}
.app {
  height: 100%;
  display: flex;
  flex-direction: column;
}
</style>
