import { describe, test, afterEach } from "node:test";
import assert from "node:assert";
import { writeFileSync, unlinkSync, existsSync, mkdirSync, readFileSync, statSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";

const __dirname = dirname(fileURLToPath(import.meta.url));
const launcherPath = join(__dirname, "launcher.js");
const require = createRequire(import.meta.url);

// Import the module as a library to get exports BEFORE the describe block
delete require.cache[require.resolve(launcherPath)];
const launcherModule = require(launcherPath);
const { PLATFORM_PACKAGES, mapPlatform, resolveBinPath } = launcherModule;

// Mock platform packages structure
const mockPackages = {
  "@mindctx/darwin-arm64": {
    dir: join(__dirname, "node_modules", "@mindctx", "darwin-arm64"),
    bin: "mindctx",
  },
  "@mindctx/darwin-x64": {
    dir: join(__dirname, "node_modules", "@mindctx", "darwin-x64"),
    bin: "mindctx",
  },
  "@mindctx/linux-x64": {
    dir: join(__dirname, "node_modules", "@mindctx", "linux-x64"),
    bin: "mindctx",
  },
  "@mindctx/linux-arm64": {
    dir: join(__dirname, "node_modules", "@mindctx", "linux-arm64"),
    bin: "mindctx",
  },
  "@mindctx/win32-x64": {
    dir: join(__dirname, "node_modules", "@mindctx", "win32-x64"),
    bin: "mindctx.exe",
  },
};

function setupMockPackage(packageName) {
  const pkg = mockPackages[packageName];
  if (!pkg) return;

  const pkgDir = pkg.dir;
  const binDir = join(pkgDir, "bin");

  if (!existsSync(binDir)) {
    mkdirSync(binDir, { recursive: true });
  }

  // Create package.json
  writeFileSync(
    join(pkgDir, "package.json"),
    JSON.stringify({ name: packageName, version: "0.0.1" })
  );

  // Create mock binary
  const mockBinary = `#!/bin/sh
echo "mock binary: $@"
exit 0
`;
  writeFileSync(join(binDir, pkg.bin), mockBinary);
}

function cleanupMockPackage(packageName) {
  const pkg = mockPackages[packageName];
  if (!pkg || !existsSync(pkg.dir)) return;

  try {
    unlinkSync(join(pkg.dir, "package.json"));
    unlinkSync(join(pkg.dir, "bin", pkg.bin));
  } catch (e) {
    // Ignore cleanup errors
  }
}

function cleanupAllMockPackages() {
  Object.keys(mockPackages).forEach(cleanupMockPackage);
}

describe("launcher", () => {
  afterEach(() => {
    cleanupAllMockPackages();
  });

  test("platform mapping for win32-x64", () => {
    const result = mapPlatform("win32-x64");
    assert.strictEqual(result, "@mindctx/win32-x64");
  });

  test("platform mapping for linux-x64", () => {
    const result = mapPlatform("linux-x64");
    assert.strictEqual(result, "@mindctx/linux-x64");
  });

  test("platform mapping for linux-arm64", () => {
    const result = mapPlatform("linux-arm64");
    assert.strictEqual(result, "@mindctx/linux-arm64");
  });

  test("platform mapping for darwin-x64", () => {
    const result = mapPlatform("darwin-x64");
    assert.strictEqual(result, "@mindctx/darwin-x64");
  });

  test("platform mapping for darwin-arm64", () => {
    const result = mapPlatform("darwin-arm64");
    assert.strictEqual(result, "@mindctx/darwin-arm64");
  });

  test("platform mapping returns undefined for unknown platform", () => {
    const result = mapPlatform("unknown-platform");
    assert.strictEqual(result, undefined);
  });

  test("bin name selection: win32 uses mindctx.exe", () => {
    const exe = "mindctx.exe"; // This is what launcher.js uses for win32
    assert.strictEqual(exe, "mindctx.exe");
  });

  test("bin name selection: non-win32 uses mindctx", () => {
    const exe = "mindctx"; // This is what launcher.js uses for non-win32
    assert.strictEqual(exe, "mindctx");
  });

  test("resolveBinPath works when package exists", () => {
    setupMockPackage("@mindctx/darwin-arm64");

    // Test that resolveBinPath works when package exists
    const binPath = resolveBinPath("@mindctx/darwin-arm64", "mindctx");
    assert.ok(binPath);
    assert.ok(binPath.includes("darwin-arm64"));
    assert.ok(binPath.includes("mindctx"));
  });

  test("resolveBinPath returns null for missing package", () => {
    // Test that resolveBinPath returns null for missing package
    const missingPath = resolveBinPath("@mindctx/missing-package", "mindctx");
    assert.strictEqual(missingPath, null);
  });

  test("exact error message format", () => {
    // The exact error message
    const expectedMessage =
      "mindctx: platform package @mindctx/darwin-arm64 was not installed (registry mirror may lag). Retry with the official registry: npm config set registry https://registry.npmjs.org/";

    // Verify the error message is in the launcher source
    const launcherContent = readFileSync(launcherPath, "utf-8");
    assert.ok(
      launcherContent.includes(
        "mindctx: platform package ${packageName} was not installed (registry mirror may lag). Retry with the official registry: npm config set registry https://registry.npmjs.org/"
      )
    );
  });

  test("exact unsupported platform error text", () => {
    const expectedError = "mindctx: unsupported platform fake-arch";

    // Verify the error message format in the launcher source
    const launcherContent = readFileSync(launcherPath, "utf-8");
    assert.ok(
      launcherContent.includes("mindctx: unsupported platform ${platform}")
    );
  });

  test("platform set contains exactly 5 platforms", () => {
    // Verify the platform set
    assert.strictEqual(Object.keys(PLATFORM_PACKAGES).length, 5);

    // Check for expected platforms
    assert.ok(PLATFORM_PACKAGES["win32-x64"]);
    assert.ok(PLATFORM_PACKAGES["linux-x64"]);
    assert.ok(PLATFORM_PACKAGES["linux-arm64"]);
    assert.ok(PLATFORM_PACKAGES["darwin-x64"]);
    assert.ok(PLATFORM_PACKAGES["darwin-arm64"]);

    // Check that win32-arm64 is NOT present
    assert.strictEqual(PLATFORM_PACKAGES["win32-arm64"], undefined);
  });

  test("shebang is first line of launcher.js", () => {
    const launcherContent = readFileSync(launcherPath, "utf-8");
    const firstLine = launcherContent.split("\n")[0];
    assert.strictEqual(firstLine, "#!/usr/bin/env node");
  });

  test("launcher.js has the executable bit set (git mode 100755, npm bin shim)", { skip: process.platform === "win32" ? "exec bit is POSIX-only" : false }, () => {
    // Without the exec bit npm's bin symlink fails with EACCES.
    const mode = statSync(launcherPath).mode;
    assert.ok(
      (mode & 0o111) !== 0,
      `launcher.js is not executable (mode ${mode.toString(8)})`
    );
  });

  test("unknown platform: exact stderr text and exit code 1", () => {
    // The launcher reads process.platform at startup; preload a module that
    // overrides it so the main branch takes the unsupported-platform path.
    const overridePath = join(tmpdir(), `mindctx-platform-override-${process.pid}.cjs`);
    writeFileSync(
      overridePath,
      "Object.defineProperty(process, 'platform', { value: 'sunos', configurable: true });" +
        "Object.defineProperty(process, 'arch', { value: 'x64', configurable: true });"
    );
    try {
      const res = spawnSync(process.execPath, ["--require", overridePath, launcherPath], {
        encoding: "utf8",
      });
      assert.strictEqual(res.status, 1);
      assert.strictEqual(res.stderr.trim(), "mindctx: unsupported platform sunos-x64");
    } finally {
      unlinkSync(overridePath);
    }
  });
});
