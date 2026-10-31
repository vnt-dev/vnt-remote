import type { AudioPlaybackDevice } from './api';

export const AUDIO_PLAYBACK_DISABLED = 'off';

/** Only an unset preference selects the default; a removed endpoint disables audio. */
export function resolveAudioPlaybackDevice(
  devices: AudioPlaybackDevice[],
  preference: string | null,
): string {
  if (preference === AUDIO_PLAYBACK_DISABLED) return AUDIO_PLAYBACK_DISABLED;
  const selected = devices.find((device) => device.id === preference);
  if (selected) return selected.id;
  if (preference !== null) return AUDIO_PLAYBACK_DISABLED;
  return (
    devices.find((device) => device.isDefault)?.id ?? devices[0]?.id ?? AUDIO_PLAYBACK_DISABLED
  );
}

export function connectionAudio(selection: string): {
  audioEnabled: boolean;
  audioDeviceId: string | null;
} {
  return selection === AUDIO_PLAYBACK_DISABLED
    ? { audioEnabled: false, audioDeviceId: null }
    : { audioEnabled: true, audioDeviceId: selection };
}
