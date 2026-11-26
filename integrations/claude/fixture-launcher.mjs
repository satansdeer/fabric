// Fixture only. Pin this file and invoke it with the explicitly pinned Node binary.
// No inherited provider/auth/proxy/runtime environment reaches the official ACP shim.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { realpathSync, statSync } from "node:fs";
import { isAbsolute, join } from "node:path";

assert.equal(process.argv.length, 5, "fixture-launcher.mjs SHIM NATIVE_CLAUDE LOOPBACK_BASE_URL");
const [shim, native, baseUrl] = process.argv.slice(2);
for (const path of [shim, native, process.env.HOME]) {
  assert.ok(isAbsolute(path) && realpathSync(path) === path, "Canonical private launch paths required");
}
assert.ok(statSync(process.env.HOME).isDirectory());
assert.match(baseUrl, /^http:\/\/127\.0\.0\.1:[1-9][0-9]*$/);
const home = process.env.HOME;
const model = "claude-sonnet-4-6";
const child = spawn(shim, [], {
  cwd: process.cwd(), stdio: "inherit", detached: false,
  env: {
    HOME: home, LC_ALL: "C", PATH: "/usr/bin:/bin", TMPDIR: join(home, "tmp"),
    XDG_CONFIG_HOME: join(home, "xdg-config"), XDG_CACHE_HOME: join(home, "xdg-cache"),
    XDG_DATA_HOME: join(home, "xdg-data"), XDG_STATE_HOME: join(home, "xdg-state"),
    CLAUDE_CONFIG_DIR: join(home, ".claude"), CLAUDE_CODE_EXECUTABLE: native,
    CLAUDE_AGENT_ACP_PROTOCOL: "v1",
    ANTHROPIC_API_KEY: "fabric-fixture-only-not-a-secret", ANTHROPIC_BASE_URL: baseUrl,
    ANTHROPIC_MODEL: model, ANTHROPIC_DEFAULT_SONNET_MODEL: model,
    ANTHROPIC_DEFAULT_OPUS_MODEL: model, ANTHROPIC_DEFAULT_HAIKU_MODEL: model,
    CLAUDE_CODE_SUBAGENT_MODEL: model, CLAUDE_CODE_MAX_RETRIES: "0",
    CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK: "1", CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK: "1",
    CLAUDE_CODE_DISABLE_MODEL_ACCESS_FALLBACK: "1", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
    CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: "false", CLAUDE_CODE_DISABLE_AUTO_MEMORY: "1",
    DISABLE_AUTOUPDATER: "1", DISABLE_UPDATES: "1", DISABLE_TELEMETRY: "1",
    DISABLE_ERROR_REPORTING: "1", DISABLE_GROWTHBOOK: "1", DO_NOT_TRACK: "1", OTEL_SDK_DISABLED: "true",
    GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0",
  },
});
// Keep the peer in Fabric's owned process group; never detach or translate ACP output.
for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) process.on(signal, () => child.kill(signal));
child.on("error", () => { console.error("fixture_shim_spawn_failed"); process.exitCode = 1; });
child.on("exit", (code, signal) => { process.exitCode = signal ? 1 : code ?? 1; });
