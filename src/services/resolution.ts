export interface ScreenSize {
  width: number;
  height: number;
}
export const SCREEN_RESOLUTION = 'screen';
const presets = [
  [3840, 2160],
  [2560, 1600],
  [2560, 1440],
  [1920, 1200],
  [1920, 1080],
  [1680, 1050],
  [1600, 900],
  [1440, 900],
  [1366, 768],
  [1280, 800],
  [1280, 720],
  [1024, 768],
  [800, 600],
];
export function validResolution(value: unknown): value is string {
  if (value === SCREEN_RESOLUTION) return true;
  if (typeof value !== 'string' || !/^\d+x\d+$/.test(value)) return false;
  return value.split('x').every((n) => Number(n) >= 2 && Number(n) <= 16384);
}
export function resolutionOptions(screen: ScreenSize | null) {
  if (!screen) return [{ value: SCREEN_RESOLUTION, label: '当前屏幕（无法获取尺寸）' }];
  const { width, height } = screen;
  const sizes = new Map<string, number[]>();
  for (const [w, h] of presets.flatMap(([w, h]) => [
    [w!, h!],
    [h!, w!],
  ])) {
    if (w! <= width && h! <= height && (w !== width || h !== height))
      sizes.set(`${w}x${h}`, [w!, h!]);
  }
  return [
    { value: SCREEN_RESOLUTION, label: `${width} × ${height}（当前屏幕）` },
    ...[...sizes.entries()]
      .sort((a, b) => b[1][0]! * b[1][1]! - a[1][0]! * a[1][1]!)
      .map(([value, [w, h]]) => ({ value, label: `${w} × ${h}` })),
  ];
}
export function fitResolution(value: string, screen: ScreenSize | null): string {
  if (!validResolution(value) || !screen || value === SCREEN_RESOLUTION) return SCREEN_RESOLUTION;
  const [width, height] = value.split('x').map(Number);
  return resolutionOptions(screen).some((option) => option.value === value) &&
    width! <= screen.width &&
    height! <= screen.height
    ? value
    : SCREEN_RESOLUTION;
}
export function resolutionTarget(mode: string, value: string) {
  const [targetWidth, targetHeight] =
    mode === 'none' || value === SCREEN_RESOLUTION ? [0, 0] : value.split('x').map(Number);
  return { targetWidth: targetWidth!, targetHeight: targetHeight! };
}
