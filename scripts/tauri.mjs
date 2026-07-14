import { spawnSync } from 'node:child_process';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const tauriCli = join(root, 'node_modules', '@tauri-apps', 'cli', 'tauri.js');
const remapFlags = [
  `--remap-path-prefix=${root}=<workspace>`,
  `--remap-path-prefix=${homedir()}=<user-home>`,
];
const env = { ...process.env };

if (env.CARGO_ENCODED_RUSTFLAGS) {
  env.CARGO_ENCODED_RUSTFLAGS = `${env.CARGO_ENCODED_RUSTFLAGS}\x1f${remapFlags.join('\x1f')}`;
} else {
  env.RUSTFLAGS = [env.RUSTFLAGS, ...remapFlags].filter(Boolean).join(' ');
}

const result = spawnSync(process.execPath, [tauriCli, ...process.argv.slice(2)], {
  cwd: root,
  env,
  stdio: 'inherit',
});

if (result.error) throw result.error;
process.exit(result.status ?? 1);
