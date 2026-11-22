import { createHash, createHmac, timingSafeEqual } from "node:crypto";

const ACTIONS = new Set(["opened", "edited", "reopened", "labeled"]);
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// Provider-specific validation ends here; no issue text enters durable queue state.
export function githubEvent(headers, raw, secret, now) {
  const signature = headers.get("x-hub-signature-256") || "";
  const expected = createHmac("sha256", secret).update(raw).digest();
  if (!/^sha256=[0-9a-f]{64}$/.test(signature) ||
      !timingSafeEqual(expected, Buffer.from(signature.slice(7), "hex"))) return { error: 401 };
  let body;
  try { body = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(raw)); }
  catch { return { error: 400 }; }
  if (!body || typeof body !== "object" || Array.isArray(body)) return { error: 400 };
  const kind = headers.get("x-github-event");
  if (kind === "ping") return { reply: { pong: true } };
  if (kind === "issues" && !UUID.test(headers.get("x-github-delivery") || "")) return { error: 400 };
  if (kind !== "issues" || !ACTIONS.has(body.action)) return { reply: { ignored: true } };
  const issue = body.issue;
  if (body.repository?.full_name !== "satansdeer/fabric" ||
      body.sender?.login !== "satansdeer" || issue?.user?.login !== "satansdeer" ||
      !Number.isSafeInteger(issue?.number) || issue.number < 1 ||
      issue.html_url !== `https://github.com/satansdeer/fabric/issues/${issue.number}`) return { error: 400 };
  if (issue.pull_request || issue.state !== "open" ||
      !Array.isArray(issue.labels) || !issue.labels.some(label => label?.name === "ready") ||
      (body.action === "labeled" && body.label?.name !== "ready")) return { reply: { ignored: true } };
  const updated = typeof issue.updated_at === "string" ? Date.parse(issue.updated_at) : NaN;
  if (!Number.isFinite(updated) || updated < now - 86400000 || updated > now + 300000) return { error: 400 };
  return { event: {
    schema_version: 1, source: "github", event_id: createHash("sha256").update(raw).digest("hex"),
    repo: "satansdeer/fabric", issue_number: issue.number, action: body.action,
    delivery_id: headers.get("x-github-delivery").toLowerCase(),
  } };
}
