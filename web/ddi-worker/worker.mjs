const appleTss = "https://gs.apple.com/TSS/controller?action=2";
const maxRequestSize = 2 * 1024 * 1024;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== "/tss" || url.search) {
      return new Response("Not found", { status: 404 });
    }

    const origin = request.headers.get("Origin");
    const allowedOrigins = (env.ALLOWED_ORIGINS ?? "").split(",").map(value => value.trim());
    if (!origin || !allowedOrigins.includes(origin)) {
      return new Response("Origin not allowed", { status: 403, headers: { Vary: "Origin" } });
    }

    const headers = new Headers({
      "Access-Control-Allow-Origin": origin,
      "Cache-Control": "no-store",
      Vary: "Origin",
    });
    const reply = (body, status) => new Response(body, { status, headers });

    if (request.method === "OPTIONS" && request.headers.get("Access-Control-Request-Method") === "POST") {
      headers.set("Access-Control-Allow-Methods", "POST");
      headers.set("Access-Control-Allow-Headers", "Content-Type");
      headers.set("Access-Control-Max-Age", "3600");
      return reply(null, 204);
    }
    if (request.method !== "POST") {
      headers.set("Allow", "POST, OPTIONS");
      return reply("Method not allowed", 405);
    }
    const contentType = request.headers.get("Content-Type")?.split(";")[0].trim().toLowerCase();
    if (contentType !== "text/xml" && contentType !== "application/xml") {
      return reply("Expected an XML plist", 415);
    }
    if (!request.body) {
      return reply("Missing request body", 400);
    }

    const reader = request.body.getReader();
    const chunks = [];
    let size = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > maxRequestSize) {
          await reader.cancel();
          return reply("Request too large", 413);
        }
        chunks.push(value);
      }
    } catch {
      return reply("Could not read request body", 400);
    } finally {
      reader.releaseLock();
    }
    if (size === 0) {
      return reply("Missing request body", 400);
    }

    try {
      const response = await fetch(appleTss, {
        method: "POST",
        headers: {
          "Content-Type": "text/xml; charset=utf-8",
          "User-Agent": "InetURL/1.0",
          "Cache-Control": "no-cache",
        },
        body: new Blob(chunks),
        redirect: "manual",
        signal: AbortSignal.timeout(30_000),
      });
      if (response.status >= 300 && response.status < 400) {
        return reply("Unexpected redirect from Apple signing service", 502);
      }
      headers.set("Content-Type", "text/plain; charset=utf-8");
      return reply(response.body, response.status);
    } catch {
      return reply("Could not reach Apple signing service", 502);
    }
  },
};
