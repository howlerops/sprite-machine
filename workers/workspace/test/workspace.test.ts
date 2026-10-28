import { env, runInDurableObject, SELF } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { FakeSandbox } from "../src/fake";
import type { Env, WorkspaceDO } from "../src/index";
import type { Result } from "../src/types";

declare module "cloudflare:test" {
  // eslint-disable-next-line @typescript-eslint/no-empty-object-type
  interface ProvidedEnv extends Env {}
}

let n = 0;
function ws() {
  return env.WORKSPACE.getByName(`t-${++n}`);
}

function ok<T>(r: Result<T>): T {
  if (!r.ok) throw new Error(`expected ok, got ${r.error.code}: ${r.error.message}`);
  return r.value;
}

function err<T>(r: Result<T>): { code: string; message: string } {
  if (r.ok) throw new Error("expected error, got ok");
  return r.error;
}

const IMG = "registry.cloudflare.com/acct/workspace:1";

async function started() {
  const s = ws();
  ok(await s.start({ image: IMG, instance: "standard-2" }));
  return s;
}

describe("selection", () => {
  it("uses FakeSandbox when DEV_FAKE_SANDBOX=1", async () => {
    const s = ws();
    await runInDurableObject(s, (inst: WorkspaceDO) => {
      expect(inst.port).toBeInstanceOf(FakeSandbox);
    });
  });
});

describe("lifecycle: stopped -> running -> stopped", () => {
  it("status starts stopped with nothing recorded", async () => {
    const st = ok(await ws().status());
    expect(st).toEqual({ state: "stopped", image: null, instance: null, startedAt: null, lastSnapshotId: null });
  });

  it("start moves to running and status reflects image/instance", async () => {
    const s = ws();
    expect(ok(await s.start({ image: IMG, instance: "standard-4" }))).toEqual({
      state: "running",
      image: IMG,
      instance: "standard-4",
    });
    const st = ok(await s.status());
    expect(st.state).toBe("running");
    expect(st.image).toBe(IMG);
    expect(st.instance).toBe("standard-4");
    expect(typeof st.startedAt).toBe("number");
  });

  it("start defaults the instance to standard-2", async () => {
    expect(ok(await ws().start({ image: IMG })).instance).toBe("standard-2");
  });

  it("start while running is invalid_state", async () => {
    const s = await started();
    expect(err(await s.start({ image: IMG })).code).toBe("invalid_state");
  });

  it("stop returns to stopped and is idempotent", async () => {
    const s = await started();
    expect(ok(await s.stop())).toEqual({ state: "stopped", wasRunning: true });
    expect((ok(await s.status())).state).toBe("stopped");
    expect(ok(await s.stop())).toEqual({ state: "stopped", wasRunning: false });
  });

  it("can start again after stop", async () => {
    const s = await started();
    ok(await s.stop());
    expect(ok(await s.start({ image: IMG })).state).toBe("running");
  });

  it("workspaces are isolated by name", async () => {
    const a = await started();
    const b = ws();
    expect(ok(await a.status()).state).toBe("running");
    expect(ok(await b.status()).state).toBe("stopped");
  });
});

describe("start validation", () => {
  const cases: [string, unknown][] = [
    ["missing image", {}],
    ["empty image", { image: "" }],
    ["non-string image", { image: 5 }],
    ["huge image", { image: "x".repeat(513) }],
    ["unknown instance", { image: IMG, instance: "gigantic" }],
    ["non-object request", "nope"],
    ["array request", []],
  ];
  for (const [name, req] of cases) {
    it(`rejects ${name}`, async () => {
      const s = ws();
      expect(err(await s.start(req as never)).code).toBe("invalid_argument");
      expect(ok(await s.status()).state).toBe("stopped");
    });
  }

  it("surfaces a sandbox failure as sandbox_error and stays stopped", async () => {
    const s = ws();
    await runInDurableObject(s, (inst: WorkspaceDO) => (inst.port as FakeSandbox).failNext("start", "boom"));
    expect(err(await s.start({ image: IMG }))).toEqual({ code: "sandbox_error", message: "boom" });
    expect(ok(await s.status()).state).toBe("stopped");
  });
});

describe("stop", () => {
  it("surfaces a sandbox failure as sandbox_error", async () => {
    const s = await started();
    await runInDurableObject(s, (inst: WorkspaceDO) => (inst.port as FakeSandbox).failNext("stop"));
    expect(err(await s.stop()).code).toBe("sandbox_error");
  });
});

describe("snapshot / restore", () => {
  it("snapshot requires running", async () => {
    expect(err(await ws().snapshot()).code).toBe("invalid_state");
  });

  it("snapshot records the id in status", async () => {
    const s = await started();
    const snap = ok(await s.snapshot({ name: "before-refactor" }));
    expect(snap.id).toMatch(/^snap-/);
    expect(snap.name).toBe("before-refactor");
    expect(ok(await s.status()).lastSnapshotId).toBe(snap.id);
  });

  it("snapshot validates name", async () => {
    const s = await started();
    expect(err(await s.snapshot({ name: 3 as never })).code).toBe("invalid_argument");
  });

  it("restore brings a stopped workspace back to running", async () => {
    const s = await started();
    const snap = ok(await s.snapshot());
    ok(await s.stop());
    expect(ok(await s.restore({ id: snap.id }))).toEqual({ state: "running", restoredFrom: snap.id });
    expect(ok(await s.status()).state).toBe("running");
  });

  it("restore validates id", async () => {
    expect(err(await ws().restore({} as never)).code).toBe("invalid_argument");
    expect(err(await ws().restore({ id: "" })).code).toBe("invalid_argument");
  });

  it("restore of an unknown snapshot is not_found", async () => {
    const s = ws();
    expect(err(await s.restore({ id: "snap-nope" })).code).toBe("not_found");
    expect(ok(await s.status()).state).toBe("stopped");
  });

  it("snapshot failure is sandbox_error", async () => {
    const s = await started();
    await runInDurableObject(s, (inst: WorkspaceDO) => (inst.port as FakeSandbox).failNext("snapshot"));
    expect(err(await s.snapshot()).code).toBe("sandbox_error");
  });
});

describe("backup / restoreBackup", () => {
  it("backup requires running", async () => {
    expect(err(await ws().backup({ dir: "/workspace" })).code).toBe("invalid_state");
  });

  for (const dir of ["relative/dir", "", "/a/../b", 7]) {
    it(`backup rejects dir ${JSON.stringify(dir)}`, async () => {
      const s = await started();
      expect(err(await s.backup({ dir: dir as never })).code).toBe("invalid_argument");
    });
  }

  it("backup returns a serialisable handle that restoreBackup accepts", async () => {
    const s = await started();
    const { handle } = ok(await s.backup({ dir: "/workspace", name: "turn-1" }));
    expect(handle.dir).toBe("/workspace");
    expect(handle.name).toBe("turn-1");
    const viaJson = JSON.parse(JSON.stringify(handle)) as typeof handle;
    expect(ok(await s.restoreBackup({ handle: viaJson }))).toEqual({ restored: true, dir: "/workspace" });
  });

  it("restoreBackup can target another dir", async () => {
    const s = await started();
    const { handle } = ok(await s.backup({ dir: "/workspace" }));
    expect(ok(await s.restoreBackup({ handle, dir: "/restored" })).dir).toBe("/restored");
  });

  it("restoreBackup validates the handle", async () => {
    const s = await started();
    expect(err(await s.restoreBackup({} as never)).code).toBe("invalid_argument");
    expect(err(await s.restoreBackup({ handle: { id: "x" } } as never)).code).toBe("invalid_argument");
  });

  it("restoreBackup requires running", async () => {
    const s = await started();
    const { handle } = ok(await s.backup({ dir: "/workspace" }));
    ok(await s.stop());
    expect(err(await s.restoreBackup({ handle })).code).toBe("invalid_state");
  });

  it("restoreBackup of an unknown handle is not_found", async () => {
    const s = await started();
    const handle = { id: "bak-nope", dir: "/workspace", size: 1, sha256: "a".repeat(64), format: "tar+zstd/1" };
    expect(err(await s.restoreBackup({ handle })).code).toBe("not_found");
  });

  it("backup failure is sandbox_error", async () => {
    const s = await started();
    await runInDurableObject(s, (inst: WorkspaceDO) => (inst.port as FakeSandbox).failNext("backup", "r2 down"));
    expect(err(await s.backup({ dir: "/workspace" }))).toEqual({ code: "sandbox_error", message: "r2 down" });
  });
});

describe("exec", () => {
  it("requires running", async () => {
    expect(err(await ws().exec({ cmd: ["echo", "hi"] })).code).toBe("invalid_state");
  });

  it("runs a command and returns stdout/exitCode", async () => {
    const s = await started();
    expect(ok(await s.exec({ cmd: ["echo", "hello", "world"] }))).toEqual({
      exitCode: 0,
      stdout: "hello world\n",
      stderr: "",
    });
  });

  it("passes env through", async () => {
    const s = await started();
    expect(ok(await s.exec({ cmd: ["env"], env: { SM_TOKEN: "t" } })).stdout).toBe("SM_TOKEN=t\n");
  });

  it("a non-zero exit is a successful result, not an error", async () => {
    const s = await started();
    expect(ok(await s.exec({ cmd: ["false"] })).exitCode).toBe(1);
  });

  const bad: [string, unknown][] = [
    ["missing cmd", {}],
    ["empty cmd", { cmd: [] }],
    ["string cmd", { cmd: "ls" }],
    ["non-string argv", { cmd: ["ls", 1] }],
    ["empty argv entry", { cmd: [""] }],
    ["bad env key", { cmd: ["env"], env: { "1BAD": "x" } }],
    ["non-string env value", { cmd: ["env"], env: { A: 1 } }],
    ["relative cwd", { cmd: ["ls"], cwd: "rel" }],
  ];
  for (const [name, req] of bad) {
    it(`rejects ${name}`, async () => {
      const s = await started();
      expect(err(await s.exec(req as never)).code).toBe("invalid_argument");
    });
  }

  it("sandbox failure is sandbox_error", async () => {
    const s = await started();
    await runInDurableObject(s, (inst: WorkspaceDO) => (inst.port as FakeSandbox).failNext("exec"));
    expect(err(await s.exec({ cmd: ["echo"] })).code).toBe("sandbox_error");
  });
});

describe("preview", () => {
  it("requires running", async () => {
    expect(err(await ws().preview({ port: 3000 })).code).toBe("invalid_state");
  });

  it("returns a proxy path for a valid port", async () => {
    const s = await started();
    expect(ok(await s.preview({ port: 3000 }))).toEqual({ port: 3000, path: "/preview/3000/" });
  });

  for (const port of [0, 65536, 1.5, "80", -1, undefined]) {
    it(`rejects port ${String(port)}`, async () => {
      const s = await started();
      expect(err(await s.preview({ port } as never)).code).toBe("invalid_argument");
    });
  }

  it("the fetch proxy reaches the fake port and 409s when stopped", async () => {
    const s = await started();
    const res = await s.fetch("http://do/preview/3000/index.html");
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("fake preview port=3000 path=/index.html");
    ok(await s.stop());
    expect((await s.fetch("http://do/preview/3000/")).status).toBe(409);
  });
});

describe("HTTP surface: POST /rpc/<method>", () => {
  async function rpc(id: string, method: string, body?: unknown, verb = "POST") {
    const res = await SELF.fetch(`http://example.com/w/${id}/rpc/${method}`, {
      method: verb,
      ...(body === undefined ? {} : { body: typeof body === "string" ? body : JSON.stringify(body) }),
    });
    return { status: res.status, json: (await res.json()) as Result<unknown> };
  }

  it("drives start -> status -> exec -> stop over JSON", async () => {
    const id = "http-1";
    let r = await rpc(id, "start", { image: IMG, instance: "standard-2" });
    expect(r.status).toBe(200);
    expect(r.json).toEqual({ ok: true, value: { state: "running", image: IMG, instance: "standard-2" } });

    r = await rpc(id, "status");
    expect(r.status).toBe(200);
    expect((r.json as { value: { state: string } }).value.state).toBe("running");

    r = await rpc(id, "exec", { cmd: ["echo", "x"] });
    expect((r.json as { value: { stdout: string } }).value.stdout).toBe("x\n");

    r = await rpc(id, "stop", {});
    expect(r.json).toEqual({ ok: true, value: { state: "stopped", wasRunning: true } });
  });

  it("maps error codes to HTTP statuses", async () => {
    const id = "http-2";
    expect((await rpc(id, "start", { image: "" })).status).toBe(400);
    expect((await rpc(id, "exec", { cmd: ["echo"] })).status).toBe(409);
    expect((await rpc(id, "restore", { id: "snap-nope" })).status).toBe(404);
    expect((await rpc(id, "start", "{not json")).status).toBe(400);
    expect((await rpc(id, "nosuchmethod", {})).status).toBe(404);
    expect((await rpc(id, "status", undefined, "GET")).status).toBe(400);
  });

  it("an error body is a typed result", async () => {
    const r = await rpc("http-3", "start", { image: "" });
    expect(r.json).toEqual({
      ok: false,
      error: { code: "invalid_argument", message: "image must be a non-empty string" },
    });
  });
});
