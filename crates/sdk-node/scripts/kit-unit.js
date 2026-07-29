const assert = require("node:assert/strict");
const test = require("node:test");

const { createClient } = require("@solana/kit");
const kit = require("@solana/surfpool/kit");

const { createSurfnetCheatcodesRpc, surfpool } = kit;

const ENDPOINT = "http://127.0.0.1:19999";

/**
 * Replaces globalThis.fetch with a handler that receives the parsed JSON-RPC
 * request and returns the JSON-RPC `result` (or `error`) for it. Returns a
 * restore function.
 */
function mockFetch(handler) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (_url, init) => {
    const request = JSON.parse(init.body);
    const outcome = handler(request);
    const envelope = { jsonrpc: "2.0", id: request.id, ...outcome };
    return new Response(JSON.stringify(envelope), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  };
  return () => {
    globalThis.fetch = originalFetch;
  };
}

test("cheatcodes RPC prefixes method names and unwraps { context, value } envelopes", async () => {
  const seenMethods = [];
  const restore = mockFetch((request) => {
    seenMethods.push(request.method);
    return { result: { context: { slot: 7 }, value: null } };
  });
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    const result = await rpc.resetNetwork().send();
    assert.deepEqual(seenMethods, ["surfnet_resetNetwork"]);
    assert.equal(result, null);
  } finally {
    restore();
  }
});

test("cheatcodes RPC passes bare (non-enveloped) results through untouched", async () => {
  const restore = mockFetch(() => ({
    result: {
      absoluteSlot: 42,
      blockHeight: 40,
      epoch: 0,
      slotIndex: 42,
      slotsInEpoch: 432000,
      transactionCount: 5,
    },
  }));
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    const result = await rpc.timeTravel({ absoluteSlot: 42 }).send();
    // Kit's default transport parses all JSON integers as bigint.
    assert.equal(result.absoluteSlot, 42n);
    assert.equal(result.transactionCount, 5n);
  } finally {
    restore();
  }
});

test("cheatcodes RPC serializes bigint request params as plain JSON integers", async () => {
  const bodies = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (_url, init) => {
    bodies.push(init.body);
    const id = JSON.parse(init.body).id;
    return new Response(
      JSON.stringify({ jsonrpc: "2.0", id, result: { context: { slot: 1 }, value: null } }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  };
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    await rpc
      .setAccount("SurfpoolTestAccount1111111111111111111111111", {
        lamports: 18446744073709551615n,
      })
      .send();
    assert.match(bodies[0], /"lamports":18446744073709551615[,}]/);
    assert.doesNotMatch(bodies[0], /\$n/);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("cheatcodes RPC parses u64 response integers above 2^53 without precision loss", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (_url, init) => {
    const id = JSON.parse(init.body).id;
    const body = `{"jsonrpc":"2.0","id":${JSON.stringify(id)},"result":{"context":{"slot":1},"value":{"pubkey1":{"lamports":50000000000000000,"owner":"11111111111111111111111111111111","executable":false,"rentEpoch":18446744073709551615,"data":"","parsedData":null}}}}`;
    return new Response(body, {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  };
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    const snapshot = await rpc.exportSnapshot().send();
    assert.equal(snapshot.pubkey1.rentEpoch, 18446744073709551615n);
    assert.equal(snapshot.pubkey1.lamports, 50_000_000_000_000_000n);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("cheatcodes RPC does not mistake a result with a `value` field for an envelope", async () => {
  const restore = mockFetch(() => ({ result: { value: "not-an-envelope" } }));
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    const result = await rpc.getSurfnetInfo().send();
    assert.deepEqual(result, { value: "not-an-envelope" });
  } finally {
    restore();
  }
});

test("cheatcodes RPC surfaces JSON-RPC errors with code, message, and data", async () => {
  const restore = mockFetch(() => ({
    error: { code: -32000, message: "boom", data: "extra" },
  }));
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    await assert.rejects(
      rpc.resetNetwork().send(),
      /Surfnet RPC error -32000: boom — extra/,
    );
  } finally {
    restore();
  }
});

test("cheatcodes RPC surfaces structured error data containing integers", async () => {
  // The transport parses all response integers as bigint, so the error
  // detail must be serialized bigint-safely.
  const restore = mockFetch(() => ({
    error: { code: -32602, message: "Invalid params", data: { expectedSlot: 123 } },
  }));
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT);
    await assert.rejects(
      rpc.resetNetwork().send(),
      /Surfnet RPC error -32602: Invalid params — \{"expectedSlot":123\}/,
    );
  } finally {
    restore();
  }
});

test("cheatcodes RPC sends configured extra headers", async () => {
  const seenHeaders = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (_url, init) => {
    seenHeaders.push(init.headers);
    const id = JSON.parse(init.body).id;
    return new Response(
      JSON.stringify({ jsonrpc: "2.0", id, result: { context: { slot: 1 }, value: null } }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  };
  try {
    const rpc = createSurfnetCheatcodesRpc(ENDPOINT, {
      headers: { authorization: "Bearer test-token" },
    });
    await rpc.resetNetwork().send();
    assert.equal(seenHeaders[0].authorization, "Bearer test-token");
    assert.equal(seenHeaders[0]["content-type"], "application/json");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("attach mode installs the full client surface without loading the native module", () => {
  const fakePayer = { address: "SurfpoolTestPayer11111111111111111111111111" };
  const client = createClient({ payer: fakePayer }).use(
    surfpool({ rpcUrl: "http://127.0.0.1:8899" }),
  );

  assert.equal(client.rpcUrl, "http://127.0.0.1:8899");
  assert.equal(client.wsUrl, "ws://127.0.0.1:8900");
  assert.equal(typeof client.cheatcodes.setAccount, "function");
  assert.equal(typeof client.rpc.getSlot, "function");
  assert.equal(typeof client.rpcSubscriptions.slotNotifications, "function");
  assert.equal(typeof client.airdrop, "function");
  assert.equal(typeof client.getMinimumBalance, "function");
  assert.equal(typeof client.transactionPlanner, "function");
  assert.equal(typeof client.transactionPlanExecutor, "function");
  assert.equal(typeof client.sendTransaction, "function");
  assert.equal(typeof client.sendTransactions, "function");
  assert.equal(client.surfnet, undefined);
  assert.equal(client.payer, fakePayer);
});

test("attach mode defaults the WebSocket URL to surfpool's default WS port", () => {
  const fakePayer = { address: "SurfpoolTestPayer11111111111111111111111111" };

  // Surfpool's WebSocket port (default 8900) is independent of its HTTP
  // port, so a custom --port keeps subscriptions on 8900.
  const customPort = createClient({ payer: fakePayer }).use(
    surfpool({ rpcUrl: "http://127.0.0.1:12345" }),
  );
  assert.equal(customPort.wsUrl, "ws://127.0.0.1:8900");

  // Port-less URLs (e.g. behind a proxy) only swap the protocol.
  const proxied = createClient({ payer: fakePayer }).use(
    surfpool({ rpcUrl: "https://surfpool.example.com" }),
  );
  assert.equal(proxied.wsUrl, "wss://surfpool.example.com");

  const explicit = createClient({ payer: fakePayer }).use(
    surfpool({
      rpcUrl: "http://127.0.0.1:12345",
      rpcSubscriptionsUrl: "ws://127.0.0.1:54321",
    }),
  );
  assert.equal(explicit.wsUrl, "ws://127.0.0.1:54321");
});

test("ESM and CJS builds expose the same named exports", async () => {
  const esm = await import("@solana/surfpool/kit");
  const cjsKeys = Object.keys(kit).filter((k) => k !== "__esModule");
  const esmKeys = Object.keys(esm).filter((k) => k !== "default" && k !== "module.exports");
  assert.deepEqual(esmKeys.sort(), cjsKeys.sort());
  assert.equal(typeof esm.surfpool, "function");
});
