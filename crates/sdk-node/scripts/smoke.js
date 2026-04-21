const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { Surfnet } = require("../dist");

const SPL_TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const NATIVE_MINT = "So11111111111111111111111111111111111111112";
const STARTUP_AIRDROP_LAMPORTS = 1_000_000_000;

test("startWithConfig supports custom payer, extra airdrops, instanceId, and drainEvents", async () => {
  const payer = Surfnet.newKeypair();
  const extra = Surfnet.newKeypair();
  const surfnet = Surfnet.startWithConfig({
    offline: true,
    blockProductionMode: "manual",
    airdropSol: STARTUP_AIRDROP_LAMPORTS,
    airdropAddresses: [extra.publicKey],
    payerSecretKey: Uint8Array.from(payer.secretKey),
  });

  assert.equal(surfnet.payer, payer.publicKey);
  assert.match(surfnet.rpcUrl, /^http:\/\//);
  assert.match(surfnet.wsUrl, /^ws:\/\//);
  assert.ok(surfnet.instanceId.length > 0, "expected instanceId");

  assert.equal(await getBalance(surfnet.rpcUrl, payer.publicKey), STARTUP_AIRDROP_LAMPORTS);
  assert.equal(await getBalance(surfnet.rpcUrl, extra.publicKey), STARTUP_AIRDROP_LAMPORTS);

  const events = surfnet.drainEvents();
  assert.ok(Array.isArray(events));
});

test("SOL account helpers can set and reset account state", async () => {
  const surfnet = Surfnet.start();
  const address = Surfnet.newKeypair().publicKey;
  const owner = Surfnet.newKeypair().publicKey;

  surfnet.fundSol(address, 42_000);
  assert.equal(await getBalance(surfnet.rpcUrl, address), 42_000);

  surfnet.setAccount(address, 77_777, Uint8Array.from([0xaa, 0xbb, 0xcc]), owner);
  const accountInfo = await getAccountInfo(surfnet.rpcUrl, address);
  assert.equal(accountInfo.owner, owner);
  assert.equal(accountInfo.lamports, 77_777);
  assert.deepEqual(
    Array.from(Buffer.from(accountInfo.data[0], "base64")),
    [0xaa, 0xbb, 0xcc],
  );

  surfnet.resetAccount(address, { includeOwnedAccounts: false });
  assert.equal(await getBalance(surfnet.rpcUrl, address), 0);
  assert.equal(await getOptionalAccountInfo(surfnet.rpcUrl, address), null);
});

test("token helpers cover ATA derivation, funding, and advanced token-account updates", async () => {
  const surfnet = Surfnet.start();
  const owner = Surfnet.newKeypair().publicKey;
  const delegate = Surfnet.newKeypair().publicKey;
  const closeAuthority = Surfnet.newKeypair().publicKey;

  surfnet.fundToken(owner, NATIVE_MINT, 55, SPL_TOKEN_PROGRAM);
  const ata = surfnet.getAta(owner, NATIVE_MINT, SPL_TOKEN_PROGRAM);
  assert.equal(await getTokenAmount(surfnet.rpcUrl, ata), "55");

  surfnet.setTokenAccount(
    owner,
    NATIVE_MINT,
    {
      amount: 321,
      delegate,
      delegatedAmount: 123,
      closeAuthority,
      state: "initialized",
    },
    SPL_TOKEN_PROGRAM,
  );

  const parsedAccount = await getParsedAccountInfo(surfnet.rpcUrl, ata);
  const info = parsedAccount.data.parsed.info;
  assert.equal(info.tokenAmount.amount, "321");
  assert.equal(info.delegate, delegate);
  assert.equal(info.delegatedAmount.amount, "123");
  assert.equal(info.closeAuthority, closeAuthority);
});

test("streamAccount registers streamed accounts via RPC introspection", async () => {
  const surfnet = Surfnet.start();
  const address = Surfnet.newKeypair().publicKey;

  surfnet.streamAccount(address, { includeOwnedAccounts: true });
  const streamedAccounts = await surfnetRpc(surfnet.rpcUrl, "surfnet_getStreamedAccounts");
  const accounts = streamedAccounts.value?.accounts ?? [];

  assert.ok(
    accounts.some(
      (account) =>
        account.pubkey === address && account.includeOwnedAccounts === true,
    ),
  );
});

test("deploy() accepts explicit bytes and deployProgram() discovers workspace artifacts", async () => {
  const surfnet = Surfnet.start();
  const explicitProgramId = Surfnet.newKeypair().publicKey;
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "surfpool-sdk-node-"));
  const idlPath = path.join(tempDir, "program.json");

  try {
    fs.writeFileSync(
      idlPath,
      JSON.stringify(sampleIdl(explicitProgramId)),
    );

    const deployedProgramId = surfnet.deploy({
      programId: explicitProgramId,
      soBytes: Uint8Array.from([1, 2, 3, 4, 5, 6]),
      idlPath,
    });

    assert.equal(deployedProgramId, explicitProgramId);
    const explicitAccount = await getAccountInfo(surfnet.rpcUrl, explicitProgramId);
    assert.equal(explicitAccount.executable, true);

    const workspace = path.join(tempDir, "workspace");
    const deployDir = path.join(workspace, "target", "deploy");
    const idlDir = path.join(workspace, "target", "idl");
    const artifactKeypair = Surfnet.newKeypair();
    const programName = "fixture_program";
    const previousCwd = process.cwd();

    fs.mkdirSync(deployDir, { recursive: true });
    fs.mkdirSync(idlDir, { recursive: true });
    fs.writeFileSync(
      path.join(deployDir, `${programName}.so`),
      Buffer.from([9, 8, 7, 6]),
    );
    fs.writeFileSync(
      path.join(deployDir, `${programName}-keypair.json`),
      JSON.stringify(Array.from(artifactKeypair.secretKey)),
    );
    fs.writeFileSync(
      path.join(idlDir, `${programName}.json`),
      JSON.stringify(sampleIdl(artifactKeypair.publicKey)),
    );

    process.chdir(workspace);
    try {
      const discoveredProgramId = surfnet.deployProgram(programName);
      assert.equal(discoveredProgramId, artifactKeypair.publicKey);
    } finally {
      process.chdir(previousCwd);
    }

    const discoveredAccount = await getAccountInfo(
      surfnet.rpcUrl,
      artifactKeypair.publicKey,
    );
    assert.equal(discoveredAccount.executable, true);
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
});

function sampleIdl(programId) {
  return {
    address: programId,
    metadata: {
      name: "test_program",
      version: "0.1.0",
      spec: "0.1.0",
      description: "Created with Anchor",
    },
    instructions: [],
    accounts: [],
    types: [],
    events: [],
    errors: [],
    constants: [],
  };
}

async function getBalance(rpcUrl, address) {
  const result = await surfnetRpc(rpcUrl, "getBalance", [
    address,
    { commitment: "processed" },
  ]);
  return result.value;
}

async function getTokenAmount(rpcUrl, address) {
  const result = await surfnetRpc(rpcUrl, "getTokenAccountBalance", [
    address,
    { commitment: "processed" },
  ]);
  return result.value.amount;
}

async function getOptionalAccountInfo(rpcUrl, address) {
  const result = await surfnetRpc(rpcUrl, "getAccountInfo", [
    address,
    { encoding: "base64", commitment: "processed" },
  ]);
  return result.value;
}

async function getAccountInfo(rpcUrl, address) {
  const value = await getOptionalAccountInfo(rpcUrl, address);
  assert.notEqual(value, null, `expected account ${address} to exist`);
  return value;
}

async function getParsedAccountInfo(rpcUrl, address) {
  const result = await surfnetRpc(rpcUrl, "getAccountInfo", [
    address,
    { encoding: "jsonParsed", commitment: "processed" },
  ]);
  assert.notEqual(result.value, null, `expected parsed account ${address} to exist`);
  return result.value;
}

async function surfnetRpc(rpcUrl, method, params = []) {
  const response = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: `${method}-${Date.now()}`,
      method,
      params,
    }),
  });

  const payload = await response.json();
  if (payload.error) {
    throw new Error(`${method}: ${payload.error.message}`);
  }
  return payload.result;
}
