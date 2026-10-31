import { describe, expect, it } from 'vitest';
import { formatBytes, formatEta, formatSpeed, progressPercent } from './format';

describe('formatBytes', () => {
  it('uses binary units and drops noise decimals', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(-1)).toBe('0 B');
    expect(formatBytes(Number.NaN)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1024)).toBe('1.0 KB');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(10 * 1024)).toBe('10 KB');
    expect(formatBytes(1024 * 1024 * 1.25)).toBe('1.3 MB');
    expect(formatBytes(1024 ** 3 * 2)).toBe('2.0 GB');
  });
});

describe('formatSpeed', () => {
  it('switches to Mbps above 1000 kbps', () => {
    expect(formatSpeed(0)).toBe('');
    expect(formatSpeed(750)).toBe('750 Kbps');
    expect(formatSpeed(4500)).toBe('4.5 Mbps');
    expect(formatSpeed(Number.NaN)).toBe('');
  });
});

describe('formatEta', () => {
  it('scales units and rounds up partial seconds', () => {
    expect(formatEta(0)).toBe('');
    expect(formatEta(-5)).toBe('');
    expect(formatEta(0.4)).toBe('1 秒');
    expect(formatEta(18)).toBe('18 秒');
    expect(formatEta(59)).toBe('59 秒');
    expect(formatEta(60)).toBe('1 分');
    expect(formatEta(150)).toBe('2 分 30 秒');
    expect(formatEta(3600)).toBe('1 小时 0 分');
    expect(formatEta(3900)).toBe('1 小时 5 分');
  });
});

describe('progressPercent', () => {
  it('clamps to 0..100 and tolerates unknown totals', () => {
    expect(progressPercent(0, 100)).toBe(0);
    expect(progressPercent(50, 100)).toBe(50);
    expect(progressPercent(99.6, 100)).toBe(100);
    expect(progressPercent(200, 100)).toBe(100);
    expect(progressPercent(10, 0)).toBe(0);
  });
});
