import { DurableObject } from "cloudflare:workers";
import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";
import { createHandler } from "../service.mjs";

globalThis.Buffer = Buffer;

export class SQLiteStore {
  constructor(storage) {
    this.storage = storage;
    storage.sql.exec("CREATE TABLE IF NOT EXISTS route_state (key TEXT PRIMARY KEY, json TEXT NOT NULL, etag TEXT NOT NULL)");
  }

  getWithMetadata(key, { type, consistency }) {
    if (type !== "json" || consistency !== "strong") throw new Error("Unsupported read");
    const row = this.storage.sql.exec("SELECT json, etag FROM route_state WHERE key = ?", key).toArray()[0];
    return row ? { data: JSON.parse(row.json), etag: row.etag } : null;
  }

  setJSON(key, data, options) {
    const json = JSON.stringify(data);
    const etag = randomUUID();
    return this.storage.transactionSync(() => {
      let rows;
      if (options.onlyIfNew === true) {
        rows = this.storage.sql.exec(
          "INSERT INTO route_state (key, json, etag) VALUES (?, ?, ?) ON CONFLICT(key) DO NOTHING RETURNING etag",
          key, json, etag,
        ).toArray();
      } else if (typeof options.onlyIfMatch === "string") {
        rows = this.storage.sql.exec(
          "UPDATE route_state SET json = ?, etag = ? WHERE key = ? AND etag = ? RETURNING etag",
          json, etag, key, options.onlyIfMatch,
        ).toArray();
      } else throw new Error("Conditional write required");
      return rows.length ? { modified: true, etag } : { modified: false };
    });
  }
}

export class ConnectInbox extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.store = new SQLiteStore(ctx.storage);
    this.handle = createHandler({ store: this.store, env, now: Date.now });
  }

  fetch(request) {
    return this.handle(request);
  }
}

export default {
  async fetch(request, env) {
    try {
      // Keep the deployed object identity: changing it would strand acknowledged v1 events.
      // Each claimed automation has its own CAS record inside this existing SQLite object.
      return await env.CONNECT_INBOX.getByName("satansdeer/fabric/issues").fetch(request);
    } catch {
      return new Response('{"error":"Service unavailable"}', {
        status: 503, headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" },
      });
    }
  },
};
