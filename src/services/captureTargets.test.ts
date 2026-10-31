import { describe, expect, it } from 'vitest';
import { captureTargets, toggleTarget } from './captureTargets';
import type { AppConfig, MonitorInfo } from './api';

const monitors = [{ index: 0, name: 'display-a' }] as MonitorInfo[];
describe('capture target selection', () => {
  it('preserves an explicitly empty selection', () => {
    expect(captureTargets({ selectedTargets: [] } as unknown as AppConfig, monitors)).toEqual([]);
  });
  it('keeps order and allows mixed overlapping targets', () => {
    const first = { kind: 'monitor' as const, name: 'display-a' };
    const region = { kind: 'region' as const, id: 'region-a' };
    const selected = toggleTarget([first], region);
    expect(selected).toEqual([first, region]);
    expect(toggleTarget(selected, first)).toEqual([region]);
  });
  it('defaults to the primary display even when it is enumerated second', () => {
    const displays = [
      { index: 0, name: 'secondary', primary: false },
      { index: 1, name: 'primary', primary: true },
    ] as MonitorInfo[];
    expect(
      captureTargets(
        { selectedTargets: [{ kind: 'monitor', name: 'offline' }] } as AppConfig,
        displays,
      ),
    ).toEqual([{ kind: 'monitor', name: 'primary' }]);
    expect(
      captureTargets(
        { selectedTargets: [{ kind: 'monitor', name: 'secondary' }] } as AppConfig,
        displays,
      ),
    ).toEqual([{ kind: 'monitor', name: 'secondary' }]);
  });
  it('removes stale display records and falls back only when all selected displays are gone', () => {
    const stale = { kind: 'monitor' as const, name: 'offline' };
    const live = { kind: 'monitor' as const, name: 'display-a' };
    const region = { kind: 'region' as const, id: 'region-a' };
    expect(captureTargets({ selectedTargets: [live, stale] } as AppConfig, monitors)).toEqual([
      live,
    ]);
    expect(captureTargets({ selectedTargets: [stale] } as AppConfig, monitors)).toEqual([live]);
    expect(captureTargets({ selectedTargets: [stale, region] } as AppConfig, monitors)).toEqual([
      region,
    ]);
    expect(captureTargets({ selectedTargets: [stale] } as AppConfig, [])).toEqual([]);
  });
});
