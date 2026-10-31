// Tauri invoke 封装与类型定义
import { invoke } from '@tauri-apps/api/core';
import { loadClipboardSyncPref, loadColorDepthPref, type ColorDepth } from './prefs';

export type Transport = 'hybrid' | 'tcp' | 'quic' | 'vnt';
export type ResolutionMode = 'none' | 'scale' | 'change';

export interface LinkConfig {
  id: string;
  name: string;
  deviceName: string;
  enabled: boolean;
  mode: string;
  subscription: string;
  servers: string[];
  networkCode: string;
  connectionId: string;
  encryptionPassword: string;
}
export interface OnlineDevice {
  id: string;
  name: string;
  connectionId: string;
  direct: boolean;
}
export interface LinkStatus {
  phase: string;
  error: string | null;
  networkCode: string;
  deviceId: string;
  localConnectionId: string;
  devices: OnlineDevice[];
}
export interface ProfileStatus extends LinkStatus {
  configId: string;
  name: string;
  enabled: boolean;
}
export interface AppConfig {
  servers: LinkConfig[];
  passwordAction?: 'keep' | 'set' | 'clear';
  formatVersion: 1;
  selectedTargets: CaptureTarget[];
  password: string;
  passwordConfigured: boolean;
  port: number;
  screenRegions: ScreenRegion[];
  clipboard: boolean;
  audio: boolean;
  hardwareEncode: boolean;
  /** 允许主控端操作本机，默认开启。 */
  allowControl: boolean;
  captureBorder: boolean;
}

export type CaptureTarget = { kind: 'monitor'; name: string } | { kind: 'region'; id: string };
export interface RemoteScreen {
  streamId: number;
  name: string;
  width: number;
  height: number;
  fps: number;
  codec: string;
  hardware: boolean;
  available: boolean;
  reason: string;
  generation: number;
}

export interface ScreenRegion {
  id: string;
  name: string;
  monitorName: string;
  screenWidth: number;
  screenHeight: number;
  orientation: number;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface MonitorInfo {
  orientation: number;
  index: number;
  width: number;
  height: number;
  name: string;
  hmonitor: number;
  primary: boolean;
}

export interface AudioPlaybackDevice {
  id: string;
  name: string;
  isDefault: boolean;
}

export interface ServerStatus {
  /** Windows 服务生命周期；state / running 表示被控实例。 */
  serviceState?: string;
  installed: boolean;
  monitors: MonitorInfo[];
  state: string;
  autoStart: boolean;
  passwordConfigured: boolean;
  consoleSession: number | null;
  desktop: string;
  error: string | null;
  running: boolean;
  port: number;
  sessions: ServerSessionInfo[];
}

export interface ServerSessionInfo {
  audioEnabled: boolean;
  transport: Transport;
  sessionId: number;
  peer: string;
  codec: string;
  hardware: boolean;
  encoderName: string;
  width: number;
  height: number;
  /** 仅共享屏幕（观看模式）会话。 */
  viewOnly: boolean;
}

export interface ClientSessionInfo {
  capabilities?: number;
  fecEnabled?: boolean;
  fecReason?: string;
  windowStreamId?: number;
  clipboardEnabled?: boolean;
  screens: RemoteScreen[];
  hostEdition: 'installed' | 'portable';
  resolutionMode: ResolutionMode;
  resolutionReason: string;
  targetWidth: number;
  targetHeight: number;
  audioEnabled: boolean;
  audioReason: string;
  clipboardConnectionId: string;
  transport: Transport;
  bitrateKbps: number;
  wsPort: number;
  token: string;
  codec: string;
  hardware: boolean;
  encoderName: string;
  width: number;
  height: number;
  fps: number;
  sessionId: number;
  /** 被控端「仅共享屏幕」：本会话只能观看，键鼠与剪贴板均不可用。 */
  viewOnly: boolean;
}

export interface ClientFecStatus {
  sessionId: number;
  recovered: number;
  ratioPermille: number;
  rawLossPermille: number | null;
  streams: {
    streamId: number;
    audio: boolean;
    ratioPermille: number;
    expected: number;
    received: number;
    recovered: number;
    residual: number;
    parityBytes: number;
  }[];
}

export interface ClientAudioStatus {
  sessionId: number;
  enabled: boolean;
  available: boolean;
  reason: string;
  bufferMs: number;
}

/** 当前连接所有远端 TCP/UDP 流量，包括视频、音频、输入、剪贴板与文件传输。 */
export interface ClientTraffic {
  sessionId: number;
  connectionId: string;
  uploadBytesPerSecond: number;
  downloadBytesPerSecond: number;
}

export interface ClientStats {
  sessionId: number;
  rttMs: number;
  serverFps: number | null;
  serverBitrateKbps: number | null;
  captureMs?: number | null;
  encodeMs?: number | null;
  targetFps: number | null;
  targetBitrateKbps: number | null;
  appliedFps: number | null;
  appliedBitrateKbps: number | null;
  lossPermille: number | null;
}

export interface ScreenInputTarget {
  streamId: number;
  generation: number;
  revision: number;
}

export type InputEventArg =
  | { type: 'mouseMove'; x: number; y: number }
  | { type: 'mouseButton'; button: number; down: boolean }
  | { type: 'wheel'; dx: number; dy: number }
  | { type: 'key'; scancode: number; extended: boolean; down: boolean };

/** 剪贴板文件传输方向：本机 → 对端 / 对端 → 本机。 */
export type TransferDirection = 'send' | 'receive';
/** 传输状态：进行中 / 完成 / 失败 / 已取消。 */
export type TransferState = 'active' | 'done' | 'failed' | 'cancelled';

/** Rust `clipboard://transfer` 事件负载。 */
export interface ClipboardTransfer {
  connectionId: string;
  sessionId: number;
  transferId: number;
  direction: TransferDirection;
  state: TransferState;
  /** 当前文件名（多文件时为正在传输的那个）或顶层条目名。 */
  name: string;
  fileIndex: number;
  fileCount: number;
  completedBytes: number;
  totalBytes: number;
  speedKbps: number;
  reason?: string;
}

export interface ConnectionParams {
  clipboardEnabled?: boolean;
  serverConfigId?: string;
  serverNodeId?: string;
  serverNetworkCode?: string;
  colorDepth?: ColorDepth;
  showWallpaper?: boolean;
  dragFullWindows?: boolean;
  fecEnabled?: boolean;
  resolutionMode: ResolutionMode;
  targetWidth: number;
  targetHeight: number;
  audioEnabled: boolean;
  audioDeviceId: string | null;
  transport: Transport;
  host: string;
  port: number;
  password: string;
  codecs: string[];
  hardwareCodecs?: string[];
  fps: number;
  bitrateKbps: number;
}
let connectionParams: ConnectionParams | undefined;

export interface PlatformInfo {
  platform: 'windows' | 'macos';
  fileClipboard: boolean;
  physicalResolution: boolean;
  screenRecording: boolean;
  accessibility: boolean;
}
export const api = {
  platformInfo: () => invoke<PlatformInfo>('platform_info'),
  requestPlatformPermission: (permission: 'screenRecording' | 'accessibility', settings = false) =>
    invoke<void>('request_platform_permission', { permission, settings }),
  pickScreenRegion: () => invoke<ScreenRegion | null>('pick_screen_region'),
  appEdition: () => invoke<'installed' | 'portable'>('app_edition'),
  computerName: () => invoke<string>('computer_name'),
  desktopUpdateHandoff: (preparing: boolean) =>
    invoke<void>('desktop_update_handoff', { preparing }),
  openReleasePage: () => invoke<void>('open_release_page'),
  getConfig: () => invoke<AppConfig>('get_config'),
  setConfig: (config: AppConfig) => invoke<void>('set_config', { config }),
  serverLinkStatus: () => invoke<ProfileStatus[]>('server_link_status'),
  serverLinkApply: (config: LinkConfig) => invoke<void>('server_link_apply', { config }),
  serverLinkRemove: (configId: string) => invoke<void>('server_link_remove', { configId }),
  serverStart: () => invoke<void>('server_start'),
  serverStop: () => invoke<void>('server_stop'),
  serverStatus: () => invoke<ServerStatus>('server_status'),
  serverSetAutoStart: (enabled: boolean) => invoke<void>('server_set_auto_start', { enabled }),
  clientSendSas: (sessionId: number) => invoke<void>('client_send_sas', { sessionId }),
  currentScreenSize: () => invoke<import('./resolution').ScreenSize | null>('current_screen_size'),
  listMonitors: () => invoke<MonitorInfo[]>('list_monitors'),
  listAudioPlaybackDevices: () => invoke<AudioPlaybackDevice[]>('list_audio_playback_devices'),
  /** 本机局域网出口地址（无法确定时返回错误，前端降级为仅显示端口）。 */
  localIp: () => invoke<string>('local_ip'),
  openControlWindow: (params: ConnectionParams) =>
    invoke<string>('open_control_window', {
      params: {
        ...params,
        clipboardEnabled: params.clipboardEnabled ?? loadClipboardSyncPref(),
        colorDepth: params.colorDepth ?? loadColorDepthPref(),
      },
    }),
  controlWindowParams: () => invoke<ConnectionParams>('control_window_params'),
  clientConnect: async (
    host: string,
    port: number,
    password: string,
    codecs: string[],
    fps = 30,
    bitrateKbps = 30000,
    transport: Transport = 'hybrid',
    audioEnabled = true,
    audioDeviceId: string | null = null,
    resolutionMode: ResolutionMode = 'none',
    targetWidth = 0,
    targetHeight = 0,
    fecEnabled = false,
    showWallpaper = true,
    dragFullWindows = false,
    colorDepth: ColorDepth = loadColorDepthPref(),
    hardwareCodecs?: string[],
    serverNodeId?: string,
    serverNetworkCode?: string,
    serverConfigId?: string,
    clipboardEnabled = loadClipboardSyncPref(),
  ) => {
    const params = {
      clipboardEnabled,
      colorDepth,
      showWallpaper,
      dragFullWindows,
      fecEnabled: transport !== 'tcp' && fecEnabled,
      ...(serverNodeId
        ? { serverNodeId, serverNetworkCode, ...(serverConfigId ? { serverConfigId } : {}) }
        : {}),
      host,
      port,
      password,
      codecs,
      ...(hardwareCodecs === undefined ? {} : { hardwareCodecs }),
      fps,
      bitrateKbps,
      transport,
      audioEnabled,
      audioDeviceId,
      resolutionMode,
      targetWidth,
      targetHeight,
    };
    const info = await invoke<ClientSessionInfo>('client_connect', { params });
    connectionParams = {
      ...params,
      targetWidth: info.targetWidth,
      targetHeight: info.targetHeight,
    };
    return info;
  },
  clientRetryH264: async (sessionId: number) => {
    if (!connectionParams) throw new Error('缺少重连参数');
    const params = { ...connectionParams, codecs: ['h264'] };
    await invoke<void>('client_disconnect', { sessionId });
    for (let attempt = 0; ; attempt++) {
      try {
        return await invoke<ClientSessionInfo>('client_connect', { params });
      } catch (error) {
        if (
          params.resolutionMode !== 'change' ||
          attempt >= 19 ||
          !String(error).includes('被控显示器正由其他主控修改分辨率或等待恢复')
        )
          throw error;
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
    }
  },
  detachScreen: (sessionId: number, streamId: number) =>
    invoke<string>('detach_screen', { sessionId, streamId }),
  screenWindowContext: () =>
    invoke<{
      session: ClientSessionInfo;
      streamId: number;
      peer: string;
      clipboardEnabled: boolean;
    }>('screen_window_context'),
  clientDisconnect: (sessionId?: number) => invoke<void>('client_disconnect', { sessionId }),
  clientSetKeyboardCapture: (
    sessionId: number,
    enabled: boolean,
    area?: { left: number; top: number; right: number; bottom: number },
    target?: ScreenInputTarget,
  ) =>
    invoke<boolean>('client_set_keyboard_capture', {
      sessionId,
      enabled,
      area,
      ...(target ? { target } : {}),
    }),
  clientSendInput: (event: InputEventArg, sessionId: number) =>
    invoke<void>('client_send_input', { event, sessionId }),
  // 批量上行：一次 IPC 送达一批有序事件，避免逐事件 invoke 的队头阻塞
  clientSendInputBatch: (events: InputEventArg[], sessionId: number, target?: ScreenInputTarget) =>
    invoke<void>('client_send_input_batch', { events, sessionId, ...(target ? { target } : {}) }),
  clientRequestKeyframe: (sessionId?: number, streamId?: number, generation?: number) =>
    invoke<void>('client_request_keyframe', { sessionId, streamId, generation }),
  clientViewScreen: (sessionId: number, streamId: number, generation: number) =>
    invoke<void>('client_view_screen', { sessionId, streamId, generation }),
  clientScreenCommand: (sessionId: number, streamId: number, revision: number, close = false) =>
    invoke<void>('client_screen_command', { sessionId, streamId, revision, close }),
  /** 取消一个剪贴板文件传输（进行中才有效）。 */
  clipboardCancelTransfer: (connectionId: string, transferId: number) =>
    invoke<void>('clipboard_cancel_transfer', { connectionId, transferId }),
  clientClipboardEnabled: (connectionId: string) =>
    invoke<boolean>('client_clipboard_enabled', { connectionId }),
  /** 本会话剪贴板同步开关（运行时，不持久化）。 */
  clientSetClipboardEnabled: (connectionId: string, enabled: boolean) =>
    invoke<void>('client_set_clipboard_enabled', { connectionId, enabled }),
};
