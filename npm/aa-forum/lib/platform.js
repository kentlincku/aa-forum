'use strict';
// Which prebuilt aaf binary this machine needs, and where npm put it.
const path = require('path');

const PACKAGES = {
  'darwin-arm64': 'aa-forum-darwin-arm64',
  'linux-x64': 'aa-forum-linux-x64',
  'linux-arm64': 'aa-forum-linux-arm64',
};

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function packageName() {
  const key = platformKey();
  const name = PACKAGES[key];
  if (!name) {
    throw new Error(
      `aa-forum has no prebuilt aaf for ${key} (supported: ${Object.keys(PACKAGES).join(', ')}).\n` +
        'Build it from source instead: cargo install --git https://github.com/kentlincku/aa-forum aaf'
    );
  }
  return name;
}

/** Absolute path of the native binary from the platform package. */
function binaryPath() {
  const name = packageName();
  let dir;
  try {
    dir = path.dirname(require.resolve(`${name}/package.json`));
  } catch {
    throw new Error(
      `${name} is not installed (were optional dependencies skipped?). Reinstall with: npm i -g aa-forum`
    );
  }
  return path.join(dir, 'bin', 'aaf');
}

module.exports = { PACKAGES, platformKey, packageName, binaryPath };
