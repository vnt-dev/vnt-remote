import { execFileSync } from 'node:child_process';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { appendDownloadTable, findAndroidRelease } from './release-notes.mjs';

vi.mock('node:child_process', () => ({ execFileSync: vi.fn() }));

const desktop = {
  body: '版本说明',
  assets: [
    { name: 'VNT_0.1.0_x64-setup.exe', url: 'https://example.com/setup.exe' },
    { name: 'VNT-windows-x64-portable.exe', url: 'https://example.com/portable.exe' },
    { name: 'VNT-macos-arm64-portable.app.zip', url: 'https://example.com/macos.zip' },
  ],
};
const android = {
  draft: false,
  published_at: '2026-11-01T00:00:00Z',
  assets: ['arm64-v8a', 'x86_64', 'universal'].map((arch) => ({
    name: `VNTRemote-${arch}-release.apk`,
    browser_download_url: `https://example.com/${arch}.apk`,
  })),
};

describe('release download table', () => {
  it('includes published APKs and ignores other Android assets', () => {
    const body = appendDownloadTable(desktop, {
      ...android,
      assets: [...android.assets, { name: 'SHA256SUMS' }],
    });
    expect(body).toContain('| Windows | x64 | 安装版 |');
    expect(body).toContain('| macOS 14+ | Apple Silicon (arm64) |');
    for (const asset of android.assets) expect(body).toContain(asset.browser_download_url);
    expect(body).toContain('通用 (arm64-v8a / x86_64)');
    expect(body).not.toContain('SHA256SUMS');
  });

  it.each([
    null,
    { ...android, draft: true },
    { ...android, published_at: null },
    { ...android, assets: [] },
  ])('keeps desktop links without a published Android APK: %j', (release) => {
    const body = appendDownloadTable(desktop, release);
    expect(body).toContain('https://example.com/setup.exe');
    expect(body).not.toContain('| Android');
  });

  it('updates an existing table and preserves surrounding notes on reruns', () => {
    const initial = appendDownloadTable(desktop) + '\n## 使用说明\n保留后续说明\n';
    const body = appendDownloadTable({ ...desktop, body: initial }, android);
    expect(body.match(/## 系统版本与下载/g)).toHaveLength(1);
    expect(body).toContain(android.assets[0].browser_download_url);
    expect(body).toContain('版本说明\n\n');
    expect(body).toContain('\n## 使用说明\n保留后续说明\n');
    expect(appendDownloadTable({ ...desktop, body }, android)).toBe(body);
  });

  it('still rejects missing desktop artifacts', () => {
    expect(() => appendDownloadTable({ ...desktop, assets: [] }, android)).toThrow(
      'Expected one Windows 安装版 asset',
    );
  });
});

describe('Android release lookup', () => {
  beforeEach(() => {
    vi.mocked(execFileSync).mockReset();
  });

  it('queries the exact version tag in the Android repository', () => {
    vi.mocked(execFileSync).mockReturnValue(JSON.stringify(android));
    expect(findAndroidRelease('vnt-dev/vnt-remote-app', 'v0.1.0')).toEqual(android);
    expect(execFileSync).toHaveBeenCalledWith(
      'gh',
      ['api', 'repos/vnt-dev/vnt-remote-app/releases/tags/v0.1.0'],
      expect.any(Object),
    );
  });

  it('skips a missing release', () => {
    vi.mocked(execFileSync).mockImplementation(() => {
      throw Object.assign(new Error('Not Found'), { stderr: 'gh: Not Found (HTTP 404)' });
    });
    expect(findAndroidRelease('vnt-dev/vnt-remote-app', 'v0.1.0')).toBeNull();
  });

  it('reports other lookup failures', () => {
    vi.mocked(execFileSync).mockImplementation(() => {
      throw Object.assign(new Error('Forbidden'), { stderr: 'gh: Forbidden (HTTP 403)' });
    });
    expect(() => findAndroidRelease('vnt-dev/vnt-remote-app', 'v0.1.0')).toThrow('Forbidden');
  });
});
