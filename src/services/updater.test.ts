import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DownloadEvent } from '@tauri-apps/plugin-updater';

const check = vi.hoisted(() => vi.fn());
const desktopUpdateHandoff = vi.hoisted(() => vi.fn<(preparing: boolean) => Promise<void>>());
vi.mock('@tauri-apps/plugin-updater', () => ({ check }));
vi.mock('./api', () => ({
  api: { appEdition: vi.fn(async () => 'installed'), desktopUpdateHandoff },
}));

function candidate() {
  return {
    version: '0.2.0',
    body: 'Release notes',
    download: vi.fn<(onEvent: (event: DownloadEvent) => void) => Promise<void>>(async () => {}),
    install: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
  };
}

describe('application updater', () => {
  beforeEach(() => {
    vi.resetModules();
    check.mockReset();
    desktopUpdateHandoff.mockReset().mockResolvedValue(undefined);
  });

  it('never checks for installer updates in the portable edition', async () => {
    const { api } = await import('./api');
    vi.mocked(api.appEdition).mockResolvedValueOnce('portable');
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    await updater.downloadUpdate();
    await updater.installUpdate();
    expect(check).not.toHaveBeenCalled();
    expect(updater.state.phase).toBe('idle');
  });

  it('keeps in-flight progress across consumers and prevents duplicate operations', async () => {
    const update = candidate();
    let finish!: () => void;
    update.download.mockImplementation(async (onEvent) => {
      onEvent({ event: 'Started', data: { contentLength: 100 } });
      onEvent({ event: 'Progress', data: { chunkLength: 40 } });
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
      onEvent({ event: 'Progress', data: { chunkLength: 60 } });
      onEvent({ event: 'Finished' });
    });
    check.mockResolvedValue(update);
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    const downloading = updater.downloadUpdate();
    const secondView = (await import('./updater')).updater;
    expect(secondView.state.phase).toBe('downloading');
    expect(secondView.percent.value).toBe(40);
    await secondView.checkForUpdates();
    await secondView.downloadUpdate();
    await secondView.installUpdate();
    expect(check).toHaveBeenCalledOnce();
    expect(update.download).toHaveBeenCalledOnce();
    expect(update.install).not.toHaveBeenCalled();
    finish();
    await downloading;
    expect(secondView.state.phase).toBe('ready');
    expect(secondView.percent.value).toBe(100);
    await updater.checkForUpdates();
    expect(check).toHaveBeenCalledOnce();
    await updater.installUpdate();
    expect(update.install).toHaveBeenCalledOnce();
    expect(desktopUpdateHandoff).toHaveBeenLastCalledWith(true);
  });

  it('does not launch an installer before service handoff succeeds', async () => {
    const update = candidate();
    check.mockResolvedValue(update);
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    await updater.downloadUpdate();
    desktopUpdateHandoff.mockRejectedValueOnce(new Error('service unavailable'));
    await updater.installUpdate();
    expect(update.install).not.toHaveBeenCalled();
    expect(desktopUpdateHandoff.mock.calls).toEqual([[true], [false]]);
    expect(updater.state.phase).toBe('ready');
    expect(updater.state.error).toContain('service unavailable');
  });

  it('does not allow installation after a signature or download failure and can retry', async () => {
    const update = candidate();
    update.download.mockRejectedValueOnce(new Error('invalid signature'));
    check.mockResolvedValue(update);
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    await updater.downloadUpdate();
    expect(updater.state.phase).toBe('available');
    expect(updater.state.error).toContain('invalid signature');
    await updater.installUpdate();
    expect(update.install).not.toHaveBeenCalled();
    await updater.downloadUpdate();
    expect(updater.state.phase).toBe('ready');
    expect(updater.state.error).toBe('');
  });

  it('retains the verified download when starting the installer fails', async () => {
    const update = candidate();
    update.install.mockRejectedValueOnce(new Error('permission denied'));
    check.mockResolvedValue(update);
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    await updater.downloadUpdate();
    await updater.installUpdate();
    expect(updater.state.phase).toBe('ready');
    expect(updater.state.error).toContain('permission denied');
    expect(desktopUpdateHandoff.mock.calls).toEqual([[true], [false]]);
    await updater.installUpdate();
    expect(update.install).toHaveBeenCalledTimes(2);
    expect(update.download).toHaveBeenCalledOnce();
  });

  it('supports unknown download length until the verified download completes', async () => {
    const update = candidate();
    update.download.mockImplementation(async (onEvent) => {
      onEvent({ event: 'Started', data: {} });
      onEvent({ event: 'Progress', data: { chunkLength: 512 } });
      expect(updater.percent.value).toBeUndefined();
      expect(updater.state.downloaded).toBe(512);
    });
    check.mockResolvedValue(update);
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    await updater.downloadUpdate();
    expect(updater.percent.value).toBe(100);
  });

  it('handles check errors, latest versions and closes superseded resources', async () => {
    const update = candidate();
    check
      .mockRejectedValueOnce(new Error('offline'))
      .mockResolvedValueOnce(update)
      .mockResolvedValueOnce(null);
    const { updater } = await import('./updater');
    await updater.checkForUpdates();
    expect(updater.state.phase).toBe('idle');
    expect(updater.state.error).toContain('offline');
    await updater.checkForUpdates();
    expect(updater.state.version).toBe('0.2.0');
    await updater.checkForUpdates();
    expect(update.close).toHaveBeenCalledOnce();
    expect(updater.state.phase).toBe('latest');
    expect(updater.state.version).toBe('');
    expect(updater.state.error).toBe('');
  });
});
