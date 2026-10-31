import process from 'node:process';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, URL } from 'node:url';

if (process.platform !== 'darwin' || process.arch !== 'arm64') {
  throw new Error('macOS development bundles require Apple Silicon');
}
const root = fileURLToPath(new URL('../', import.meta.url));
const result = spawnSync(
  process.execPath,
  [
    'node_modules/@tauri-apps/cli/tauri.js',
    'build',
    '--debug',
    '--config',
    'src-tauri/tauri.portable.conf.json',
    '--config',
    'src-tauri/tauri.macos.conf.json',
    '--',
    '--locked',
  ],
  {
    cwd: root,
    stdio: 'inherit',
    env: { ...process.env, MACOSX_DEPLOYMENT_TARGET: '14.0' },
  },
);
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);
const signature = spawnSync('codesign', ['-d', '-r-', 'target/debug/bundle/macos/vnt-remote.app'], {
  cwd: root,
  encoding: 'utf8',
});
if (`${signature.stdout ?? ''}${signature.stderr ?? ''}`.includes('designated => cdhash')) {
  process.stderr.write(
    '\n当前应用使用 ad-hoc 签名，修改并重新构建后可能需要重新授权屏幕录制和辅助功能。\n' +
      '若系统设置显示已授权但应用检测未开启，请移除旧条目，再添加并授权当前 .app。\n' +
      '只需重新打开时，请使用 npm run start:macos，避免重复构建。\n\n',
  );
}
if (!process.argv.includes('--build-only')) {
  const opened = spawnSync('open', ['target/debug/bundle/macos/vnt-remote.app'], {
    cwd: root,
    stdio: 'inherit',
  });
  if (opened.error) throw opened.error;
  if (opened.status !== 0) process.exit(opened.status ?? 1);
}
