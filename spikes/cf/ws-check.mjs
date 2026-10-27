// WebSocket checks for test.sh (Node 22 has a global WebSocket).
// Usage: node ws-check.mjs ws://127.0.0.1:PORT
// Exits non-zero on any mismatch or after a 15 s timeout.
const base = process.argv[2];
if (!base) throw new Error("usage: node ws-check.mjs ws://host:port");

setTimeout(() => {
  console.error("ws-check: timed out");
  process.exit(1);
}, 15_000).unref();

function open(url) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    const inbox = [];
    const waiters = [];
    ws.addEventListener("message", (e) => {
      const w = waiters.shift();
      if (w) w(String(e.data));
      else inbox.push(String(e.data));
    });
    ws.addEventListener("open", () =>
      resolve({
        ws,
        next: () =>
          inbox.length
            ? Promise.resolve(inbox.shift())
            : new Promise((r) => waiters.push(r)),
      }),
    );
    ws.addEventListener("error", () => reject(new Error(`cannot open ${url}`)));
  });
}

async function expectReply(conn, send, want) {
  conn.ws.send(send);
  const got = await conn.next();
  if (got !== want) throw new Error(`sent ${JSON.stringify(send)}: want ${JSON.stringify(want)}, got ${JSON.stringify(got)}`);
  console.log(`ws ok: ${send} -> ${got}`);
}

const a = await open(`${base}/ws/room-a`);
// Echo goes through Room::websocket_message (the DO wakes up).
await expectReply(a, "hello", "echo:hello");
// "ping" would be echoed as "echo:ping" by websocket_message; getting "pong" proves the
// runtime answered via setWebSocketAutoResponse instead.
await expectReply(a, "ping", "pong");
await expectReply(a, "again", "echo:again");

// A second socket on another DO instance works independently.
const b = await open(`${base}/ws/room-b`);
await expectReply(b, "ping", "pong");
await expectReply(b, "b-msg", "echo:b-msg");

// Clean close: the DO answers the close frame from websocket_close.
await new Promise((resolve) => {
  a.ws.addEventListener("close", (e) => {
    console.log(`ws ok: close code ${e.code}`);
    resolve();
  });
  a.ws.close(1000, "done");
});
b.ws.close(1000, "done");
console.log("ws-check: all passed");
process.exit(0);
