import { AWARD, HTTP_AUTH } from "../src/model/kinds.js";
import {
  bytes,
  Evidence,
  Event,
  hash,
  now,
  one,
  outgoing,
  NEMO,
  RELAY_HTTP,
  verify,
} from "../src/try/wire.js";
export interface Envelope {
  eventBody: string;
  relayAuth: Event;
  evidence?: Evidence;
}
export async function forward(
  raw: string,
  enabled: boolean,
  fetcher: typeof fetch = fetch,
  time = now(),
  seller = NEMO,
): Promise<Response> {
  if (!enabled) return Response.json({ error: "Not found" }, { status: 404 });
  if (bytes(raw) > 65536)
    return Response.json({ error: "Request too large" }, { status: 413 });
  let forwarding = false;
  try {
    const body = JSON.parse(raw) as Envelope;
    if (typeof body.eventBody !== "string") throw Error("Invalid envelope");
    const event = JSON.parse(body.eventBody) as Event,
      a = body.relayAuth;
    outgoing(event, body.evidence, seller);
    verify(a);
    if (event.kind === AWARD && time > body.evidence!.offer!.created_at + 300)
      return Response.json({ error: "Award deadline passed" }, { status: 409 });
    if (
      a.kind !== HTTP_AUTH ||
      a.content !== "" ||
      a.pubkey !== event.pubkey ||
      Math.abs(time - a.created_at) > 60 ||
      a.tags.length !== 4 ||
      JSON.stringify(one(a, "u")) !== JSON.stringify(["u", RELAY_HTTP]) ||
      JSON.stringify(one(a, "method")) !== JSON.stringify(["method", "POST"]) ||
      JSON.stringify(one(a, "payload")) !==
        JSON.stringify(["payload", hash(body.eventBody)]) ||
      !/^[a-f0-9]{32}$/.test(one(a, "nonce")[1] ?? "")
    )
      throw Error("Invalid relay authentication");
    // Old IDs may be reconciled via reads, never re-created or freshly forwarded.
    if (Math.abs(time - event.created_at) > 60)
      return Response.json(
        { error: "Reconcile the original event ID" },
        { status: 409 },
      );
    forwarding = true;
    const response = await fetcher(RELAY_HTTP, {
      method: "POST",
      redirect: "error",
      signal: AbortSignal.timeout(8000),
      headers: {
        "Content-Type": "application/json",
        Authorization: `Nostr ${Buffer.from(JSON.stringify(a)).toString("base64")}`,
      },
      body: body.eventBody,
    });
    if (response.status === 429)
      return Response.json(
        { error: "Temporarily rate limited" },
        {
          status: 429,
          headers: {
            "Retry-After": response.headers.get("Retry-After") ?? "60",
          },
        },
      );
    if (!response.ok)
      return Response.json({ error: "Relay unavailable" }, { status: 502 });
    const receipt = await response.json();
    if (receipt.accepted !== true || receipt.event_id !== event.id)
      return Response.json(
        { error: "Relay did not acknowledge this event" },
        { status: 502 },
      );
    return Response.json({ accepted: true, event_id: event.id });
  } catch {
    return Response.json(
      { error: forwarding ? "Relay unavailable" : "Invalid request" },
      { status: forwarding ? 502 : 400 },
    );
  }
}
// Vercel's Web-standard function entry point. No framework or server signing key.
export default {
  async fetch(request: Request) {
    if (
      process.env.TRY_IT_API_ENABLED !== "true" ||
      process.env.TRY_IT_ENABLED !== "true"
    )
      return Response.json({ error: "Not found" }, { status: 404 });
    if (request.method !== "POST")
      return new Response(null, { status: 405, headers: { Allow: "POST" } });
    const reader = request.body?.getReader();
    let raw = "";
    let size = 0;
    const decoder = new TextDecoder();
    if (reader) {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        size += chunk.value.length;
        if (size > 65536) {
          await reader.cancel();
          return new Response(null, { status: 413 });
        }
        raw += decoder.decode(chunk.value, { stream: true });
      }
      raw += decoder.decode();
    }
    return forward(raw, true);
  },
};
