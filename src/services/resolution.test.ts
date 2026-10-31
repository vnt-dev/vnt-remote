import { describe, expect, it } from 'vitest';
import { fitResolution, resolutionOptions, resolutionTarget, validResolution } from './resolution';
describe('target resolution', () => {
  it('puts exact physical dimensions first and filters, deduplicates and sorts presets', () => {
    const screen = { width: 2560, height: 1440 };
    const options = resolutionOptions(screen);
    expect(options[0]).toEqual({ value: 'screen', label: '2560 × 1440（当前屏幕）' });
    const sizes = options.slice(1).map((o) => o.value.split('x').map(Number));
    expect(new Set(options.map((o) => o.value)).size).toBe(options.length);
    expect(sizes.every(([w, h]) => w! <= screen.width && h! <= screen.height)).toBe(true);
    const areas = sizes.map(([w, h]) => w! * h!);
    expect(areas).toEqual([...areas].sort((a, b) => b - a));
    expect(options.some((o) => o.value === '1920x1080')).toBe(true);
    expect(options.some((o) => o.value === '2560x1440')).toBe(false);
  });
  it('supports portrait, unusual sizes and small screens without CSS pixel conversion', () => {
    expect(
      resolutionOptions({ width: 1080, height: 1920 }).some((o) => o.value === '720x1280'),
    ).toBe(true);
    expect(resolutionOptions({ width: 2256, height: 1504 })[0]!.label).toContain('2256 × 1504');
    expect(resolutionOptions({ width: 400, height: 300 })).toHaveLength(1);
  });
  it('resets unavailable or oversized choices and preserves valid lower choices', () => {
    expect(fitResolution('1920x1080', { width: 1280, height: 720 })).toBe('screen');
    expect(fitResolution('1280x720', { width: 1920, height: 1080 })).toBe('1280x720');
    expect(fitResolution('1920x1080', null)).toBe('screen');
    expect(validResolution('0x100')).toBe(false);
    expect(validResolution('99999x100')).toBe(false);
  });
  it('ignores remembered choices for none and leaves screen resolution for native resolution', () => {
    expect(resolutionTarget('none', '1280x720')).toEqual({ targetWidth: 0, targetHeight: 0 });
    expect(resolutionTarget('scale', 'screen')).toEqual({ targetWidth: 0, targetHeight: 0 });
    for (const mode of ['scale', 'change'])
      expect(resolutionTarget(mode, '1280x720')).toEqual({ targetWidth: 1280, targetHeight: 720 });
  });
});
