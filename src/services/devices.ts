import { parseAddr } from './address';
import { connectionIdLabel } from './linkProfiles';
import { validResolution } from './resolution';
import type { ResolutionMode, Transport } from './api';
import type { ColorDepth } from './prefs';

export interface DeviceAdvanced {
  colorDepth: ColorDepth;
  resolutionMode: ResolutionMode;
  targetResolution?: string;
  showWallpaper: boolean;
  dragFullWindows: boolean;
  hwDecode: boolean;
  clipboardSync: boolean;
  audioPlaybackDevice: string | null;
}
export interface SavedDevice extends DeviceAdvanced {
  id: string;
  name: string;
  addr: string;
  password: string;
  transport: Transport;
  serverConfigId?: string;
  serverNodeId?: string;
  serverNetworkCode?: string;
  fecEnabled: boolean;
  protocol: string; // auto | h264 | hevc | av1
  fps: number;
  bitrateKbps: number;
  lastUsed: number; // epoch ms，0 表示从未连接
}

/** Read the complete device-list format. */
export function parseDevices(raw: string | null): SavedDevice[] {
  if (raw === null) return [];
  const value = JSON.parse(raw);
  if (
    !value ||
    value.formatVersion !== 1 ||
    !Array.isArray(value.devices) ||
    !value.devices.every(validDevice)
  )
    throw new Error('设备列表格式无效');
  return value.devices;
}
function validDevice(d: unknown): d is SavedDevice {
  if (!d || typeof d !== 'object') return false;
  const value = d as Record<string, unknown>;
  return (
    ['id', 'name', 'addr', 'password'].every((key) => typeof value[key] === 'string') &&
    !!value.id &&
    typeof value.addr === 'string' &&
    !!(value.serverNodeId ? connectionIdLabel(value.addr) : parseAddr(value.addr)) &&
    ['hybrid', 'tcp', 'quic', 'vnt'].includes(String(value.transport)) &&
    ['auto', 'h264', 'hevc', 'av1'].includes(String(value.protocol)) &&
    [15, 16, 24, 32].includes(Number(value.colorDepth)) &&
    typeof value.colorDepth === 'number' &&
    ['none', 'scale', 'change'].includes(String(value.resolutionMode)) &&
    (value.targetResolution === undefined || validResolution(value.targetResolution)) &&
    ['fecEnabled', 'showWallpaper', 'dragFullWindows', 'hwDecode', 'clipboardSync'].every(
      (key) => typeof value[key] === 'boolean',
    ) &&
    (value.audioPlaybackDevice === null || typeof value.audioPlaybackDevice === 'string') &&
    [30, 60].includes(Number(value.fps)) &&
    typeof value.fps === 'number' &&
    typeof value.bitrateKbps === 'number' &&
    Number.isFinite(value.bitrateKbps) &&
    value.bitrateKbps >= 100 &&
    value.bitrateKbps <= 200000 &&
    typeof value.lastUsed === 'number' &&
    Number.isFinite(value.lastUsed) &&
    value.lastUsed >= 0 &&
    ['serverConfigId', 'serverNodeId', 'serverNetworkCode'].every(
      (key) => value[key] === undefined || typeof value[key] === 'string',
    )
  );
}

export function writeDevices(storage: Pick<Storage, 'setItem'>, devices: SavedDevice[]) {
  const raw = JSON.stringify({ formatVersion: 1, devices });
  storage.setItem('vnt.devices', raw);
}
