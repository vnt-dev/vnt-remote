import type { AppConfig, CaptureTarget, MonitorInfo } from './api';

export const targetKey = (target: CaptureTarget) =>
  target.kind === 'monitor' ? `monitor:${target.name}` : `region:${target.id}`;

export function captureTargets(config: AppConfig, monitors: MonitorInfo[]): CaptureTarget[] {
  const primary = monitors.find((monitor) => monitor.primary) ?? monitors[0];
  const fallback: CaptureTarget[] = primary ? [{ kind: 'monitor', name: primary.name }] : [];
  const selected = config.selectedTargets.filter(
    (target) => target.kind !== 'monitor' || monitors.some((m) => m.name === target.name),
  );
  return selected.length || !config.selectedTargets.length ? selected : fallback;
}

export function toggleTarget(selected: CaptureTarget[], target: CaptureTarget): CaptureTarget[] {
  const key = targetKey(target);
  return selected.some((item) => targetKey(item) === key)
    ? selected.filter((item) => targetKey(item) !== key)
    : [...selected, target];
}
