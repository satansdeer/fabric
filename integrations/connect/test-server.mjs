// Loopback-only integration fixture. Production always uses the Netlify store.
import { createServer } from "node:http";
import { constants } from "node:fs";
import { mkdir, open, rename, unlink } from "node:fs/promises";
import { dirname, isAbsolute } from "node:path";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { Readable } from "node:stream";
import { createHandler } from "./service.mjs";

async function readPrivateJSON(path) {
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const info = await file.stat();
    if (!info.isFile() || info.size > 1024 * 1024 || (info.mode & 0o077) || info.uid !== process.getuid()) throw new Error("Invalid fixture file");
    return JSON.parse(await file.readFile("utf8"));
  } finally { await file.close(); }
}

export class FileStore {
  constructor(path) {
    if (!isAbsolute(path)) throw new Error("Absolute fixture path required");
    this.path = path;
    this.pending = Promise.resolve();
  }
  async snapshot(key) {
    let stored;
    try { stored = await readPrivateJSON(this.path); }
    catch (error) { if (error.code === "ENOENT") return null; throw error; }
    if (stored.key !== key || typeof stored.etag !== "string") throw new Error("Invalid fixture state");
    return stored;
  }
  async getWithMetadata(key) {
    const stored = await this.snapshot(key);
    return stored && { data: stored.data, etag: stored.etag, metadata: {} };
  }
  async setJSON(key, data, options) {
    const update = this.pending.then(async () => {
      const current = await this.snapshot(key);
      if ((options.onlyIfNew && current) || (options.onlyIfMatch && current?.etag !== options.onlyIfMatch)) return { modified: false };
      if (!options.onlyIfNew && !options.onlyIfMatch) throw new Error("Conditional fixture writes required");
      const etag = randomUUID();
      const temporary = `${this.path}.${randomUUID()}.tmp`;
      await mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
      let file;
      try {
        file = await open(temporary, "wx", 0o600);
        await file.writeFile(JSON.stringify({ key, etag, data }));
        await file.sync();
        await file.close();
        file = null;
        await rename(temporary, this.path);
        const directory = await open(dirname(this.path), "r");
        try { await directory.sync(); } finally { await directory.close(); }
      } finally {
        if (file) await file.close();
        await unlink(temporary).catch(error => { if (error.code !== "ENOENT") throw error; });
      }
      return { modified: true, etag };
    });
    this.pending = update.catch(() => {});
    return update;
  }
}

async function main() {
  const options = {};
  for (let index = 2; index < process.argv.length; index += 2) {
    const name = process.argv[index];
    if (!["--port", "--state", "--secret-file"].includes(name) || options[name] !== undefined || !process.argv[index + 1]) throw new Error("Invalid fixture arguments");
    options[name] = process.argv[index + 1];
  }
  if (!/^\d+$/.test(options["--port"] || "") || Number(options["--port"]) > 65535 ||
      !isAbsolute(options["--state"] || "") || !isAbsolute(options["--secret-file"] || "") ||
      options["--state"] === options["--secret-file"]) throw new Error("Invalid fixture arguments");
  const secrets = await readPrivateJSON(options["--secret-file"]);
  const handler = createHandler({ store: new FileStore(options["--state"]), env: {
    FABRIC_CONNECT_WEBHOOK_SECRET: secrets.webhook_secret,
    FABRIC_CONNECT_WORKER_TOKEN_SHA256: secrets.worker_token_sha256,
    FABRIC_CONNECT_WORKER_ID: secrets.worker_id,
  } });
  const server = createServer(async (incoming, outgoing) => {
    try {
      const request = new Request(`http://127.0.0.1${incoming.url}`, {
        method: incoming.method, headers: incoming.headers,
        ...(["GET", "HEAD"].includes(incoming.method) ? {} : { body: Readable.toWeb(incoming), duplex: "half" }),
      });
      const response = await handler(request);
      outgoing.writeHead(response.status, Object.fromEntries(response.headers));
      outgoing.end(Buffer.from(await response.arrayBuffer()));
    } catch {
      outgoing.writeHead(503, { "content-type": "application/json", "cache-control": "no-store" });
      outgoing.end('{"error":"Service unavailable"}');
    }
  });
  server.requestTimeout = 10000;
  server.headersTimeout = 10000;
  server.listen(Number(options["--port"]), "127.0.0.1", () => {
    console.log(JSON.stringify({ service: "fabric-connect-test", port: server.address().port }));
  });
  for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => server.close(() => process.exit(0)));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => { console.error("Fixture server failed"); process.exitCode = 1; });
}
