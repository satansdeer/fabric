import { createHash, createHmac, timingSafeEqual } from "node:crypto";

const ACTIONS = new Set(["opened", "edited", "reopened", "labeled"]);
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const NAME = /^[A-Za-z0-9_.-]{1,100}$/;

export function validPolicy(policy) {
  return policy && Object.keys(policy).sort().join(",") === "author,label,repo,sender" &&
    typeof policy.repo === "string" && policy.repo.split("/").length === 2 &&
    policy.repo.split("/").every(part => NAME.test(part) && part !== "." && part !== "..") &&
    ["author", "sender", "label"].every(key => typeof policy[key] === "string" && NAME.test(policy[key]));
}

// Provider-specific validation ends here; no issue text enters durable queue state.
export function githubEvent(headers, raw, secret, now, policy) {
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
  if (body.repository?.full_name !== policy.repo ||
      body.sender?.login !== policy.sender || issue?.user?.login !== policy.author ||
      !Number.isSafeInteger(issue?.number) || issue.number < 1 || issue.number > 2147483647 ||
      issue.html_url !== `https://github.com/${policy.repo}/issues/${issue.number}`) return { error: 400 };
  if (issue.pull_request || issue.state !== "open" ||
      !Array.isArray(issue.labels) || !issue.labels.some(label => label?.name === policy.label) ||
      (body.action === "labeled" && body.label?.name !== policy.label)) return { reply: { ignored: true } };
  const updated = typeof issue.updated_at === "string" ? Date.parse(issue.updated_at) : NaN;
  if (!Number.isFinite(updated)) return { error: 400 };
  return { stale: updated < now - 86400000 || updated > now + 300000, event: {
    schema_version: 1, source: "github", event_id: createHash("sha256").update(raw).digest("hex"),
    repo: policy.repo, issue_number: issue.number, action: body.action,
    delivery_id: headers.get("x-github-delivery").toLowerCase(),
  } };
}
