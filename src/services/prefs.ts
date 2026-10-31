import { validResolution, SCREEN_RESOLUTION } from './resolution';
// 主控端全局偏好：主窗口（Home）写 localStorage，控制窗口（同源）
// 建立会话时读取，与 services/codec.ts 的硬件解码偏好同一套路。

/**
 * 剪贴板同步默认开关（主控「高级选项」）：控制端新会话按此初始化，
 * 也可在控制窗口里对单个会话临时改（仅本次生效，不回写）。
 */
const CLIPBOARD_SYNC_KEY = 'vnt.clipboardSync';

export function loadClipboardSyncPref(): boolean {
  try {
    return localStorage.getItem(CLIPBOARD_SYNC_KEY) !== '0';
  } catch {
    return true;
  }
}

export function saveClipboardSyncPref(on: boolean) {
  try {
    localStorage.setItem(CLIPBOARD_SYNC_KEY, on ? '1' : '0');
  } catch {
    // 存储不可用时仅本次生效
  }
}

// Endpoint choices affect new connections only.
export function loadShowWallpaperPref(): boolean {
  return loadBooleanPref('vnt.showWallpaper', true);
}
export function saveShowWallpaperPref(on: boolean) {
  saveBooleanPref('vnt.showWallpaper', on);
}
export function loadDragFullWindowsPref(): boolean {
  return loadBooleanPref('vnt.dragFullWindows', false);
}
export function saveDragFullWindowsPref(on: boolean) {
  saveBooleanPref('vnt.dragFullWindows', on);
}
function loadBooleanPref(key: string, fallback: boolean): boolean {
  try {
    const value = localStorage.getItem(key);
    return value === '1' ? true : value === '0' ? false : fallback;
  } catch {
    return fallback;
  }
}
function saveBooleanPref(key: string, on: boolean) {
  try {
    localStorage.setItem(key, on ? '1' : '0');
  } catch {
    // 存储不可用时仅本次生效
  }
}

const AUDIO_PLAYBACK_KEY = 'vnt.audioPlaybackDevice';

export function loadAudioPlaybackPref(): string | null {
  try {
    return localStorage.getItem(AUDIO_PLAYBACK_KEY) || null;
  } catch {
    return null;
  }
}

export function saveAudioPlaybackPref(selection: string | null) {
  try {
    localStorage.setItem(AUDIO_PLAYBACK_KEY, selection ?? '');
  } catch {
    // 存储不可用时仅本次生效
  }
}

import type { ResolutionMode } from './api';
const RESOLUTION_MODE_KEY = 'vnt.resolutionMode';
export function loadResolutionModePref(): ResolutionMode {
  try {
    const mode = localStorage.getItem(RESOLUTION_MODE_KEY);
    return mode === 'scale' || mode === 'change' ? mode : 'none';
  } catch {
    return 'none';
  }
}
export function saveResolutionModePref(mode: ResolutionMode) {
  try {
    localStorage.setItem(RESOLUTION_MODE_KEY, mode);
  } catch {
    /* 本次生效 */
  }
}

export type ColorDepth = 15 | 16 | 24 | 32;
const COLOR_DEPTH_KEY = 'vnt.colorDepth';
export function loadColorDepthPref(): ColorDepth {
  try {
    const value = localStorage.getItem(COLOR_DEPTH_KEY);
    return value === '15' ? 15 : value === '16' ? 16 : value === '32' ? 32 : 24;
  } catch {
    return 24;
  }
}
export function saveColorDepthPref(depth: ColorDepth) {
  try {
    localStorage.setItem(COLOR_DEPTH_KEY, String(depth));
  } catch {
    // 存储不可用时仅本次生效
  }
}

export function loadTargetResolutionPref(): string {
  try {
    const value = localStorage.getItem('vnt.targetResolution');
    return validResolution(value) ? value : SCREEN_RESOLUTION;
  } catch {
    return SCREEN_RESOLUTION;
  }
}
export function saveTargetResolutionPref(value: string) {
  try {
    localStorage.setItem('vnt.targetResolution', value);
  } catch {
    /* 本次生效 */
  }
}
