import { describe, expect, it } from 'vitest';
import { AUDIO_PLAYBACK_DISABLED, connectionAudio, resolveAudioPlaybackDevice } from './audio';
import type { AudioPlaybackDevice } from './api';

const devices: AudioPlaybackDevice[] = [
  { id: 'speakers', name: '扬声器', isDefault: false },
  { id: 'headset', name: '耳机', isDefault: true },
];

describe('audio playback selection', () => {
  it('selects the actual system default and preserves an explicit endpoint', () => {
    expect(resolveAudioPlaybackDevice(devices, null)).toBe('headset');
    expect(resolveAudioPlaybackDevice(devices, 'speakers')).toBe('speakers');
    expect(connectionAudio('speakers')).toEqual({ audioEnabled: true, audioDeviceId: 'speakers' });
  });
  it('disables audio after the selected device is removed', () => {
    expect(connectionAudio(resolveAudioPlaybackDevice(devices, 'removed-device'))).toEqual({
      audioEnabled: false,
      audioDeviceId: null,
    });
    expect(resolveAudioPlaybackDevice(devices, 'speakers')).toBe('speakers');
    expect(resolveAudioPlaybackDevice([{ ...devices[0]!, isDefault: false }], null)).toBe(
      'speakers',
    );
  });
  it('keeps explicit opt-out even when playback devices exist', () => {
    expect(resolveAudioPlaybackDevice(devices, AUDIO_PLAYBACK_DISABLED)).toBe(
      AUDIO_PLAYBACK_DISABLED,
    );
    expect(connectionAudio(AUDIO_PLAYBACK_DISABLED)).toEqual({
      audioEnabled: false,
      audioDeviceId: null,
    });
  });
  it('disables transmission without persisting opt-out when there are no devices', () => {
    const preference = null;
    expect(connectionAudio(resolveAudioPlaybackDevice([], preference))).toEqual({
      audioEnabled: false,
      audioDeviceId: null,
    });
    expect(resolveAudioPlaybackDevice(devices, preference)).toBe('headset');
  });
});
