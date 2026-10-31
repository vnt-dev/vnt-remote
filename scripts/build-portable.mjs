import process from 'node:process';
import { spawnSync } from 'node:child_process';
const mac = process.platform === 'darwin';
if (!mac && process.platform !== 'win32')
  throw new Error('Portable builds support Windows and macOS');
if (mac && process.arch !== 'arm64') throw new Error('macOS portable builds require Apple Silicon');
const args = ['build', '--config', 'src-tauri/tauri.portable.conf.json'];
if (mac) args.push('--config', 'src-tauri/tauri.macos.conf.json');
args.push('--', '--locked');
const cli = 'node_modules/@tauri-apps/cli/tauri.js';
const result = spawnSync(process.execPath, [cli, ...args], {
  stdio: 'inherit',
  env: { ...process.env, ...(mac ? { MACOSX_DEPLOYMENT_TARGET: '14.0' } : {}) },
});
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);
await import('./package-portable.mjs');
