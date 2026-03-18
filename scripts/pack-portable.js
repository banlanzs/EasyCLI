// Pack Windows portable zip: single exe + optional WebView2Loader.dll
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const TARGET = 'x86_64-pc-windows-msvc';
const RELEASE_DIR = path.join('src-tauri', 'target', TARGET, 'release');
const OUT_DIR = 'release-out';

// Read version from package.json
const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
const version = pkg.version;
const zipName = `EasyCLI-v${version}-windows-x64.zip`;
const exeName = `EasyCLI-v${version}-windows-x64.exe`;

const exeSrc = path.join(RELEASE_DIR, 'easycli.exe');
const dllSrc = path.join(RELEASE_DIR, 'WebView2Loader.dll');

if (!fs.existsSync(exeSrc)) {
  console.error(`ERROR: exe not found at ${exeSrc}`);
  process.exit(1);
}

fs.mkdirSync(OUT_DIR, { recursive: true });

// Copy exe with versioned name into a temp folder, then zip
const tmpDir = path.join(OUT_DIR, '_tmp_portable');
fs.mkdirSync(tmpDir, { recursive: true });

fs.copyFileSync(exeSrc, path.join(tmpDir, exeName));
if (fs.existsSync(dllSrc)) {
  fs.copyFileSync(dllSrc, path.join(tmpDir, 'WebView2Loader.dll'));
}

const destZip = path.join(OUT_DIR, zipName);
if (fs.existsSync(destZip)) fs.unlinkSync(destZip);

// Use PowerShell Compress-Archive on Windows
execSync(
  `powershell -Command "Compress-Archive -Path '${tmpDir}\\*' -DestinationPath '${destZip}'"`,
  { stdio: 'inherit' }
);

// Cleanup temp
fs.rmSync(tmpDir, { recursive: true, force: true });

console.log(`\nPortable package ready: ${destZip}`);
