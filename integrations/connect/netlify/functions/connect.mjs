import { getStore } from "@netlify/blobs";
import { createHandler } from "../../service.mjs";

export default async request => {
  try {
    return await createHandler({
      store: getStore({ name: "fabric-connect-v1", consistency: "strong" }),
      env: process.env,
    })(request);
  } catch {
    return new Response('{"error":"Service unavailable"}', {
      status: 503, headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" },
    });
  }
};

export const config = { path: ["/health", "/v1/*"] };
