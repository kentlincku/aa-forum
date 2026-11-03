'use strict';
// Fallback when install.js did not replace bin/aaf with the native binary
// (for example `npm i --ignore-scripts`): run the binary as a child process.
const { spawn } = require('child_process');
const { binaryPath } = require('./platform');

let bin;
try {
  bin = binaryPath();
} catch (e) {
  console.error(e.message);
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });

// SIGTERM sent to this process (kill, a service manager) is passed on; so is SIGHUP, as SIGTERM,
// because aaf shuts down cleanly only on SIGINT/SIGTERM. Ctrl-C (SIGINT) already reaches the
// child through the terminal's process group; forwarding it too would count as a second Ctrl-C
// and make `aaf up` exit at once. So a SIGINT sent to this process alone is ignored: stop the
// launcher with SIGTERM instead.
const forward = (sig) => () => {
  try {
    child.kill(sig);
  } catch {}
};
process.on('SIGTERM', forward('SIGTERM'));
process.on('SIGHUP', forward('SIGTERM'));
process.on('SIGINT', () => {});

child.on('error', (e) => {
  console.error(`aaf: cannot run ${bin}: ${e.message}`);
  process.exit(1);
});
child.on('exit', (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
  } else {
    process.exit(code === null ? 1 : code);
  }
});
