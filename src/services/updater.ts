import { computed, reactive, readonly } from 'vue';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { edition, loadEdition } from './edition';
import { api } from './api';

type Phase = 'idle' | 'checking' | 'available' | 'latest' | 'downloading' | 'ready' | 'installing';

// Module lifetime owns both the native update resource and progress, independent of views.
const state = reactive({
  phase: 'idle' as Phase,
  version: '',
  notes: '',
  downloaded: 0,
  total: undefined as number | undefined,
  error: '',
});
let update: Update | null = null;
const busy = computed(() => ['checking', 'downloading', 'installing'].includes(state.phase));
const percent = computed(() =>
  ['ready', 'installing'].includes(state.phase)
    ? 100
    : state.total
      ? Math.min(100, Math.floor((state.downloaded / state.total) * 100))
      : undefined,
);

async function checkForUpdates() {
  await loadEdition();
  if (edition.value !== 'installed') return;
  if (busy.value || state.phase === 'ready') return;
  state.phase = 'checking';
  state.error = '';
  try {
    if (update) await update.close();
    update = null;
    state.version = '';
    state.notes = '';
    state.downloaded = 0;
    state.total = undefined;
    update = await check({ timeout: 30_000 });
    state.version = update?.version ?? '';
    state.notes = update?.body ?? '';
    state.phase = update ? 'available' : 'latest';
  } catch (error) {
    state.phase = 'idle';
    state.error = `检查更新失败：${String(error)}`;
  }
}

async function downloadUpdate() {
  if (!update || busy.value || state.phase !== 'available') return;
  state.phase = 'downloading';
  state.downloaded = 0;
  state.total = undefined;
  state.error = '';
  try {
    await update.download(
      (event) => {
        if (event.event === 'Started') state.total = event.data.contentLength;
        if (event.event === 'Progress') state.downloaded += event.data.chunkLength;
      },
      { timeout: 30 * 60_000 },
    );
    // download() resolves only after signature verification has succeeded.
    state.phase = 'ready';
  } catch (error) {
    state.phase = 'available';
    state.error = `下载更新失败，可重试：${String(error)}`;
  }
}

async function installUpdate() {
  if (!update || state.phase !== 'ready') return;
  state.phase = 'installing';
  state.error = '';
  try {
    // Windows updater starts NSIS and exits the entire app, including remote windows.
    await api.desktopUpdateHandoff(true);
    await update.install();
  } catch (error) {
    await api.desktopUpdateHandoff(false).catch(() => {});
    state.phase = 'ready';
    state.error = `安装更新失败，可重试：${String(error)}`;
  }
}

export const updater = {
  state: readonly(state),
  busy,
  percent,
  checkForUpdates,
  downloadUpdate,
  installUpdate,
};
