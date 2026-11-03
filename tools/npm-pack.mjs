#!/usr/bin/env node
// Build the npm tarballs for a release: one platform package per prebuilt binary
// (aa-forum-<os>-<cpu>, only bin/aaf) and the main package aa-forum (launcher + install.js)
// whose optionalDependencies pin those packages to the same version.
//
//   tools/npm-pack.mjs --out dist/npm --bin darwin-arm64=path/to/aaf [--bin linux-x64=…] [--allow-partial]
//
// The version comes from Cargo.toml ([workspace.package] version). Without --allow-partial every
// supported platform must be given, so a release can never point at a package that was not built.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const require = createRequire(import.meta.url);
const { PACKAGES } = require(path.join(root, 'npm/aa-forum/lib/platform.js'));
const PLATFORM = {
  'darwin-arm64': { os: 'darwin', cpu: 'arm64' },
  'linux-x64': { os: 'linux', cpu: 'x64' },
  'linux-arm64': { os: 'linux', cpu: 'arm64' },
};

function args() {
  const a = { out: null, bins: {}, allowPartial: false };
  const v = process.argv.slice(2);
  for (let i = 0; i < v.length; i++) {
    if (v[i] === '--out') a.out = v[++i];
    else if (v[i] === '--bin') {
      const [key, file] = v[++i].split('=');
      if (!PACKAGES[key]) throw new Error(`unknown platform ${key}`);
      a.bins[key] = path.resolve(file);
    } else if (v[i] === '--allow-partial') a.allowPartial = true;
    else throw new Error(`unknown argument ${v[i]}`);
  }
  if (!a.out) throw new Error('--out is required');
  if (!Object.keys(a.bins).length) throw new Error('give at least one --bin <platform>=<file>');
  const missing = Object.keys(PACKAGES).filter((k) => !a.bins[k]);
  if (missing.length && !a.allowPartial) throw new Error(`missing --bin for ${missing.join(', ')}`);
  return a;
}

function cargoVersion() {
  const toml = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  const m = toml.match(/\[workspace\.package\][^[]*?\nversion\s*=\s*"([^"]+)"/);
  if (!m) throw new Error('no [workspace.package] version in Cargo.toml');
  return m[1];
}

const common = (version) => ({
  version,
  homepage: 'https://github.com/kentlincku/aa-forum#readme',
  repository: { type: 'git', url: 'git+https://github.com/kentlincku/aa-forum.git' },
  license: 'MIT',
});

function pack(dir, out) {
  const name = execFileSync('npm', ['pack', '--silent', '--pack-destination', out], { cwd: dir })
    .toString()
    .trim()
    .split('\n')
    .pop();
  return path.join(out, name);
}

function main() {
  const a = args();
  const version = cargoVersion();
  const out = path.resolve(a.out);
  const stage = path.join(out, 'stage');
  fs.rmSync(stage, { recursive: true, force: true });
  fs.mkdirSync(stage, { recursive: true });
  const tarballs = [];

  for (const [key, file] of Object.entries(a.bins)) {
    const name = PACKAGES[key];
    const dir = path.join(stage, name);
    fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
    fs.copyFileSync(file, path.join(dir, 'bin', 'aaf'));
    fs.chmodSync(path.join(dir, 'bin', 'aaf'), 0o755);
    fs.copyFileSync(path.join(root, 'LICENSE'), path.join(dir, 'LICENSE'));
    fs.writeFileSync(
      path.join(dir, 'README.md'),
      `# ${name}\n\nThe prebuilt \`aaf\` binary for ${key}. Install [aa-forum](https://www.npmjs.com/package/aa-forum) instead.\n`
    );
    const pkg = {
      name,
      ...common(version),
      description: `The aaf binary of aa-forum for ${key}`,
      os: [PLATFORM[key].os],
      cpu: [PLATFORM[key].cpu],
      files: ['bin/aaf', 'README.md', 'LICENSE'],
    };
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
    tarballs.push(pack(dir, out));
  }

  const main = path.join(stage, 'aa-forum');
  fs.cpSync(path.join(root, 'npm/aa-forum'), main, { recursive: true });
  fs.copyFileSync(path.join(root, 'LICENSE'), path.join(main, 'LICENSE'));
  const pkg = JSON.parse(fs.readFileSync(path.join(main, 'package.json'), 'utf8'));
  pkg.version = version;
  pkg.optionalDependencies = Object.fromEntries(Object.values(PACKAGES).map((n) => [n, version]));
  fs.writeFileSync(path.join(main, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
  tarballs.push(pack(main, out));

  for (const t of tarballs) console.log(t);
}

try {
  main();
} catch (e) {
  console.error(`npm-pack: ${e.message}`);
  process.exit(1);
}
