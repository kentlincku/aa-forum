'use strict';
// postinstall: put the native binary at bin/aaf so the `aaf` command runs it directly,
// with no Node process in between (signals reach aaf, agents' `aaf task …` calls start fast,
// and `aaf service install` records a stable path). Any failure keeps the Node launcher.
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const { binaryPath } = require('./lib/platform');

const dst = path.join(__dirname, 'bin', 'aaf');

function main() {
  let src;
  try {
    src = binaryPath();
  } catch (e) {
    console.warn(`aa-forum: ${e.message}`);
    return;
  }
  const tmp = `${dst}.${process.pid}.tmp`;
  try {
    fs.copyFileSync(src, tmp);
    fs.chmodSync(tmp, 0o755);
    // Only swap in a binary that actually runs here.
    execFileSync(tmp, ['--version'], { stdio: 'ignore', timeout: 30000 });
    fs.renameSync(tmp, dst);
  } catch (e) {
    // bin/aaf is only replaced by the final rename, so on any failure it is still the launcher.
    try {
      fs.unlinkSync(tmp);
    } catch {}
    console.warn(`aa-forum: using the Node launcher (${e.message})`);
  }
}

main();
