import { mkdirSync, copyFileSync, readFileSync, existsSync } from 'node:fs';
import { resolve } from 'node:path';
import process from 'node:process';
import { execFileSync } from 'node:child_process';

const { version } = JSON.parse(readFileSync('package.json', 'utf8'));
const root = resolve('target/release/bundle/portable');
mkdirSync(root, { recursive: true });
if (process.platform === 'darwin') {
  if (process.arch !== 'arm64') throw new Error('macOS release requires arm64');
  const app = resolve('target/release/bundle/macos/vnt-remote.app');
  if (!existsSync(app)) throw new Error(`Missing application bundle: ${app}`);
  const architecture = execFileSync(
    '/usr/bin/lipo',
    ['-archs', resolve(app, 'Contents/MacOS/vnt-remote')],
    { encoding: 'utf8' },
  ).trim();
  if (architecture !== 'arm64') throw new Error(`Expected arm64 application, got ${architecture}`);
  execFileSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', app], { stdio: 'inherit' });
  execFileSync(
    '/usr/bin/ditto',
    [
      '-c',
      '-k',
      '--sequesterRsrc',
      '--keepParent',
      app,
      resolve(root, `vnt-remote-${version}-macos-arm64-portable.app.zip`),
    ],
    { stdio: 'inherit' },
  );
} else if (process.platform === 'win32') {
  copyFileSync(
    resolve('target/release/vnt-remote.exe'),
    resolve(root, `vnt-remote-${version}-windows-x64-portable.exe`),
  );
} else {
  throw new Error('Unsupported portable platform');
}
