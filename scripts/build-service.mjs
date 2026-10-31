import process from 'node:process';
import { execFileSync } from 'node:child_process';
import { mkdirSync, copyFileSync } from 'node:fs';
import { resolve } from 'node:path';
const debug = process.argv.includes('--debug') || process.env.TAURI_ENV_DEBUG === 'true';
const host = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)?.[1];
const target = process.env.TAURI_ENV_TARGET_TRIPLE || host;
if (!target?.includes('windows'))
  throw new Error('Windows service installer requires a Windows target');
const args = ['build', '-p', 'vnt-host', '--bin', 'vnt-service'];
if (target !== host) args.push('--target', target);
if (!debug) args.push('--release');
execFileSync('cargo', args, { stdio: 'inherit' });
const directory = resolve('src-tauri/binaries');
mkdirSync(directory, { recursive: true });
copyFileSync(
  resolve(
    'target',
    ...(target === host ? [] : [target]),
    debug ? 'debug' : 'release',
    'vnt-service.exe',
  ),
  resolve(directory, `vnt-service-${target}.exe`),
);
