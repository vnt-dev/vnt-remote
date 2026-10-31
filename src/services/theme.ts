// 主题模式（浅色 / 深色 / 跟随系统）：持久化到 localStorage，
// 在 main.ts 挂载应用前先应用一次，避免首帧闪色。
import { ref } from 'vue';

export type ThemeMode = 'light' | 'dark' | 'system';

const THEME_KEY = 'vnt.theme';
const mql = window.matchMedia('(prefers-color-scheme: dark)');

function resolve(mode: ThemeMode): 'light' | 'dark' {
  return mode === 'system' ? (mql.matches ? 'dark' : 'light') : mode;
}

function apply(mode: ThemeMode) {
  document.documentElement.dataset.theme = resolve(mode);
}

function loadMode(): ThemeMode {
  try {
    const saved = localStorage.getItem(THEME_KEY);
    if (saved === 'light' || saved === 'dark' || saved === 'system') return saved;
  } catch {
    // 存储不可用时按跟随系统处理
  }
  return 'system';
}

export const themeMode = ref<ThemeMode>(loadMode());
apply(themeMode.value);
window.addEventListener('storage', (event) => {
  if (event.key === THEME_KEY || event.key === null) {
    themeMode.value = loadMode();
    apply(themeMode.value);
  }
});

mql.addEventListener('change', () => {
  if (themeMode.value === 'system') apply('system');
});

export function setThemeMode(mode: ThemeMode) {
  themeMode.value = mode;
  try {
    localStorage.setItem(THEME_KEY, mode);
  } catch {
    // 持久化失败不影响本次会话
  }
  apply(mode);
}
