import process from 'node:process';
import { spawnSync } from 'node:child_process';
if (process.platform !== 'linux' || process.arch !== 'x64') {
  throw new Error('Linux release builds require Linux x86_64');
}
const result = spawnSync(
  process.execPath,
  [
    'node_modules/@tauri-apps/cli/tauri.js',
    'build',
    '--config',
    'src-tauri/tauri.linux.conf.json',
    '--',
    '--locked',
  ],
  { stdio: 'inherit' },
);
if (result.error) throw result.error;
process.exit(result.status ?? 1);
