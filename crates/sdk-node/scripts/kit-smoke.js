const assert = require("node:assert/strict");
const test = require("node:test");

const { createClient } = require("@solana/kit");
const { Surfnet } = require("@solana/surfpool");
const { surfpool } = require("@solana/surfpool/kit");

test("embedded surfpool() boots a Surfnet and wires the full kit client", async (t) => {
  const client = await createClient().use(surfpool({ surfnet: { offline: true } }));
  t.after(() => client.surfnet.stop());

  // Payer is Surfnet's pre-funded payer account.
  assert.equal(client.payer.address, client.surfnet.payer);
  assert.equal(client.rpcUrl, client.surfnet.rpcUrl);
  assert.equal(client.wsUrl, client.surfnet.wsUrl);

  // Standard RPC stack points at the embedded instance.
  const slot = await client.rpc.getSlot().send();
  assert.equal(typeof slot, "bigint");
  const payerBalance = await client.rpc.getBalance(client.payer.address).send();
  assert.ok(payerBalance.value > 0n, "expected pre-funded payer");

  // Typed cheatcodes RPC: bare-result methods (clock family).
  const paused = await client.cheatcodes.pauseClock().send();
  assert.equal(typeof paused.absoluteSlot, "bigint");
  const target = paused.absoluteSlot + 1_000n;
  const warped = await client.cheatcodes.timeTravel({ absoluteSlot: target }).send();
  assert.equal(warped.absoluteSlot, target);
  const resumed = await client.cheatcodes.resumeClock().send();
  assert.ok(resumed.absoluteSlot >= target);

  // Enveloped methods: setAccount roundtrip against the standard RPC.
  const account = Surfnet.newKeypair().publicKey;
  const owner = Surfnet.newKeypair().publicKey;
  const setResult = await client.cheatcodes
    .setAccount(account, { data: "aabbcc", lamports: 777_777, owner })
    .send();
  assert.equal(setResult, null);
  const info = await client.rpc.getAccountInfo(account, { encoding: "base64" }).send();
  assert.equal(info.value.lamports, 777_777n);
  assert.equal(info.value.owner, owner);
  assert.equal(info.value.data[0], Buffer.from("aabbcc", "hex").toString("base64"));

  // Cheatcode access control roundtrip.
  await client.cheatcodes.disableCheatcode(["surfnet_pauseClock"]).send();
  await assert.rejects(client.cheatcodes.pauseClock().send(), /Surfnet RPC error/);
  await client.cheatcodes.enableCheatcode(["surfnet_pauseClock"]).send();
  await client.cheatcodes.pauseClock().send();
  await client.cheatcodes.resumeClock().send();

  // Enveloped getters.
  const surfnetInfo = await client.cheatcodes.getSurfnetInfo().send();
  assert.ok(Array.isArray(surfnetInfo.runbookExecutions));
  const streamed = await client.cheatcodes.getStreamedAccounts().send();
  assert.ok(Array.isArray(streamed.accounts));

  // Inherited airdrop plugin goes through the standard requestAirdrop flow.
  const recipient = Surfnet.newKeypair().publicKey;
  await client.airdrop(recipient, 1_000_000_000n);
  const funded = await client.rpc.getBalance(recipient).send();
  assert.equal(funded.value, 1_000_000_000n);
});

test("embedded surfpool() airdrops configured addresses at startup", async (t) => {
  const recipient = Surfnet.newKeypair().publicKey;
  const signerLike = { address: Surfnet.newKeypair().publicKey };
  // Funded by the Surfnet itself before the plugin runs, so the plugin's own
  // airdrop lands on an address that already holds lamports.
  const preFunded = Surfnet.newKeypair().publicKey;
  const client = await createClient().use(
    surfpool({
      airdropAddresses: [recipient, signerLike, preFunded],
      airdropAmount: 3_000_000_000n,
      surfnet: {
        airdropAddresses: [preFunded],
        airdropSol: 1_000_000_000,
        offline: true,
      },
    }),
  );
  t.after(() => client.surfnet.stop());

  const funded = await client.rpc.getBalance(recipient).send();
  assert.equal(funded.value, 3_000_000_000n);
  const fundedSigner = await client.rpc.getBalance(signerLike.address).send();
  assert.equal(fundedSigner.value, 3_000_000_000n);
  // Additive: the startup balance survives and the airdrop is added to it.
  const toppedUp = await client.rpc.getBalance(preFunded).send();
  assert.equal(toppedUp.value, 4_000_000_000n);
});

test("disposing the embedded client stops the Surfnet", async () => {
  const client = await createClient().use(surfpool({ surfnet: { offline: true } }));
  await client.rpc.getSlot().send();

  client[Symbol.dispose]();

  await assert.rejects(client.rpc.getSlot().send());
});
