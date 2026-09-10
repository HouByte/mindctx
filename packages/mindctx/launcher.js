#!/usr/bin/env node
"use strict";

const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const { dirname, join } = require("node:path");

const PLATFORM_PACKAGES = {
  "win32-x64": "@mindctx/win32-x64",
  "linux-x64": "@mindctx/linux-x64",
  "linux-arm64": "@mindctx/linux-arm64",
  "darwin-x64": "@mindctx/darwin-x64",
  "darwin-arm64": "@mindctx/darwin-arm64",
};

function mapPlatform(platform) {
  return PLATFORM_PACKAGES[platform];
}

function resolveBinPath(packageName, exe) {
  try {
    const pkgPath = require.resolve(`${packageName}/package.json`);
    const pkgDir = dirname(pkgPath);
    return join(pkgDir, "bin", exe);
  } catch {
    return null;
  }
}

// Export functions for testing when required as a module
if (require.main !== module) {
  module.exports = { PLATFORM_PACKAGES, mapPlatform, resolveBinPath };
} else {
  // Main CLI execution (only when run directly)
  const platform = `${process.platform}-${process.arch}`;
  const packageName = mapPlatform(platform);

if (!packageName) {
  console.error(`mindctx: unsupported platform ${platform}`);
  process.exit(1);
}

const exe = process.platform === "win32" ? "mindctx.exe" : "mindctx";
const binPath = resolveBinPath(packageName, exe);

if (!binPath || !existsSync(binPath)) {
  console.error(
    `mindctx: platform package ${packageName} was not installed (registry mirror may lag). Retry with the official registry: npm config set registry https://registry.npmjs.org/`
  );
  process.exit(1);
}

const isServe = process.argv[2] === "serve";
const stdio = isServe ? ["pipe", "pipe", "pipe"] : "inherit";

const child = spawn(binPath, process.argv.slice(2), { stdio });

// Forward stdio for serve mode
if (isServe) {
  process.stdin.pipe(child.stdin);
  child.stdout.pipe(process.stdout);
  child.stderr.pipe(process.stderr);
}

// Forward signals with proper cleanup
const forwardSignal = (signal) => {
  child.kill(signal);
  const timeout = setTimeout(() => {
    child.kill("SIGKILL");
  }, 5000);
  child.once("exit", () => clearTimeout(timeout));
};

process.on("SIGINT", () => forwardSignal("SIGINT"));
process.on("SIGTERM", () => forwardSignal("SIGTERM"));

child.on("error", (err) => {
  console.error(`mindctx: failed to start: ${err.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
}
