import process from 'node:process';
import { readFileSync, writeFileSync } from 'node:fs';

const config = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'));
const pkg = JSON.parse(readFileSync('package.json', 'utf8'));
const cargo = readFileSync('Cargo.toml', 'utf8');
const cargoVersion = cargo.match(/\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1];
const tag = process.env.RELEASE_TAG;
if (
  !tag ||
  tag !== `v${config.version}` ||
  config.version !== pkg.version ||
  config.version !== cargoVersion
) {
  throw new Error(
    'Release tag must be v<version>, matching package.json, Cargo.toml and tauri.conf.json',
  );
}
const repository = process.env.RELEASE_REPOSITORY || 'vnt-dev/vnt-remote';
if (!/^[\w.-]+\/[\w.-]+$/.test(repository)) throw new Error('Invalid RELEASE_REPOSITORY');
if (process.argv.includes('--portable')) process.exit(0);
if (!process.env.TAURI_SIGNING_PRIVATE_KEY || !process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
  throw new Error(
    'Configure TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD secrets',
  );
}
// Keep ordinary local builds unsigned; release builds must produce signed updater artifacts.
writeFileSync(
  'src-tauri/tauri.release.conf.json',
  JSON.stringify(
    {
      bundle: { createUpdaterArtifacts: true },
      plugins: {
        updater: {
          endpoints: [`https://github.com/${repository}/releases/latest/download/latest.json`],
        },
      },
    },
    null,
    2,
  ) + '\n',
);
