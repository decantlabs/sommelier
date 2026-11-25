// sommelier Cloudflare Worker: redacted status page, stale alarm by cron, and HA witness key (section 13.5).

const WITNESS_KEY = "ha:witness";
const STATUS_KEY = "status:snapshot";
const ALARM_KEY = "ha:alarm";
const CRON_STALE_SEC = 300; // 5 minutes (section 13.5)

function json(status, body) {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "Content-Type": "application/json",
      "Cache-Control": "no-store",
    },
  });
}

function unauthorized() {
  return json(401, { ok: false, error: "unauthorized" });
}

function checkAuth(request, env) {
  const expected = env.SOMMELIER_WITNESS_TOKEN || "";
  if (!expected) return false;
  const auth = request.headers.get("Authorization") || "";
  if (!auth.startsWith("Bearer ")) return false;
  return auth.slice(7).trim() === expected;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const nowSec = Math.floor(Date.now() / 1000);

    if (request.method === "GET" && (url.pathname === "/" || url.pathname === "/status")) {
      const snap = await env.SOMMELIER_KV.get(STATUS_KEY, { type: "json" });
      const witness = await env.SOMMELIER_KV.get(WITNESS_KEY, { type: "json" });
      const alarm = await env.SOMMELIER_KV.get(ALARM_KEY, { type: "json" });
      return json(200, {
        ok: true,
        witness,
        alarm,
        status: snap,
        as_of: nowSec,
      });
    }

    if (request.method === "PUT" && url.pathname === "/status") {
      if (!checkAuth(request, env)) return unauthorized();
      const body = await request.json();
      await env.SOMMELIER_KV.put(STATUS_KEY, JSON.stringify({ ...body, updated_at: nowSec }));
      return json(200, { ok: true, updated_at: nowSec });
    }

    if (request.method === "GET" && url.pathname === "/witness") {
      const witness = await env.SOMMELIER_KV.get(WITNESS_KEY, { type: "json" });
      const ageSec = witness ? Math.max(0, nowSec - (witness.heartbeat || 0)) : null;
      const stale = witness ? ageSec >= CRON_STALE_SEC : true;
      return json(200, { ok: true, witness, stale, age_sec: ageSec });
    }

    if (request.method === "POST" && url.pathname === "/witness/heartbeat") {
      if (!checkAuth(request, env)) return unauthorized();
      const body = await request.json();
      const holder = String(body.holder || "").trim();
      const epoch = Number(body.epoch || 0);
      const heartbeat = Number(body.heartbeat || nowSec);
      if (!holder || !Number.isInteger(epoch) || epoch < 1) {
        return json(400, { ok: false, error: "holder and positive integer epoch required" });
      }
      const cur = await env.SOMMELIER_KV.get(WITNESS_KEY, { type: "json" });
      if (cur && (epoch < cur.epoch || (epoch === cur.epoch && cur.holder !== holder))) {
        return json(409, {
          ok: false,
          error: "stale epoch",
          current_epoch: cur.epoch,
          current_holder: cur.holder,
        });
      }
      const next = { holder, epoch, heartbeat };
      await env.SOMMELIER_KV.put(WITNESS_KEY, JSON.stringify(next));
      await env.SOMMELIER_KV.delete(ALARM_KEY);
      return json(200, { ok: true, witness: next });
    }

    if (request.method === "POST" && url.pathname === "/witness/promote") {
      if (!checkAuth(request, env)) return unauthorized();
      const body = await request.json();
      const holder = String(body.holder || "").trim();
      const expectedEpoch = Number(body.expected_epoch || 0);
      const heartbeat = Number(body.heartbeat || nowSec);
      if (!holder || !Number.isInteger(expectedEpoch) || expectedEpoch < 0) {
        return json(400, { ok: false, error: "holder and expected_epoch required" });
      }
      const cur = await env.SOMMELIER_KV.get(WITNESS_KEY, { type: "json" });
      const curEpoch = cur ? Number(cur.epoch || 0) : 0;
      if (curEpoch !== expectedEpoch) {
        return json(409, {
          ok: false,
          error: "epoch mismatch",
          current_epoch: curEpoch,
        });
      }
      const next = { holder, epoch: expectedEpoch + 1, heartbeat };
      await env.SOMMELIER_KV.put(WITNESS_KEY, JSON.stringify(next));
      return json(200, { ok: true, witness: next });
    }

    return json(404, { ok: false, error: "not found" });
  },

  async scheduled(_event, env) {
    const nowSec = Math.floor(Date.now() / 1000);
    const witness = await env.SOMMELIER_KV.get(WITNESS_KEY, { type: "json" });
    if (!witness) return;
    const ageSec = Math.max(0, nowSec - Number(witness.heartbeat || 0));
    if (ageSec >= CRON_STALE_SEC) {
      await env.SOMMELIER_KV.put(
        ALARM_KEY,
        JSON.stringify({
          stale: true,
          holder: witness.holder,
          epoch: witness.epoch,
          age_sec: ageSec,
          raised_at: nowSec,
        })
      );
    }
  },
};
