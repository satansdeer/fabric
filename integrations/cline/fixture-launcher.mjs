// Fixture only: native ACP, an explicit environment, and one exclusively owned hub.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFileSync, realpathSync, statSync } from "node:fs";
import { isAbsolute, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

export function nativeEnvironment(state, port) {
  assert.ok(isAbsolute(state) && realpathSync(state) === state);
  assert.ok(Number.isInteger(port) && port >= 0 && port <= 65535);
  return {
    PATH: "/usr/bin:/bin", LC_ALL: "C", HOME: join(state, "home"), USERPROFILE: join(state, "home"),
    TMPDIR: join(state, "tmp"), CLINE_DIR: join(state, "cline"), CLINE_DATA_DIR: join(state, "data"),
    CLINE_HUB_DISCOVERY_PATH: join(state, "data", "hub.json"), CLINE_HUB_HOST: "127.0.0.1",
    CLINE_HUB_PORT: String(port), CLINE_PROVIDER: "fabric-local", CLINE_MODEL: "fabric-test",
    CLINE_API_KEY: "fabric-dummy", CLINE_NO_AUTO_UPDATE: "1", E2E_TEST: "true",
    GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0",
    OTEL_SDK_DISABLED: "true",
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  assert.equal(process.argv.length, 4, "fixture-launcher.mjs NATIVE_CLINE OWNED_LAUNCH_JSON");
  const [native, config] = process.argv.slice(2);
  for (const path of [native, config]) assert.ok(isAbsolute(path) && realpathSync(path) === path);
  const info = statSync(config);
  assert.ok(info.isFile() && info.uid === process.getuid() && !(info.mode & 0o077) && info.size <= 4096);
  const { state, port } = JSON.parse(readFileSync(config, "utf8"));
  assert.ok(port > 0);
  const child = spawn(native, ["--acp"], {
    cwd: process.cwd(), stdio: "inherit", detached: false, env: nativeEnvironment(state, port),
  });
  // Stay in Fabric's process group; never retire the hub when this peer exits.
  for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) process.on(signal, () => child.kill(signal));
  child.on("error", () => { console.error("fixture_native_spawn_failed"); process.exitCode = 1; });
  child.on("exit", (code, signal) => { process.exitCode = signal ? 1 : code ?? 1; });
}
