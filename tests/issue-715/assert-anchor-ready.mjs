const rpcUrl = process.argv[2] ?? "http://127.0.0.1:18899";
const cloneAddress =
  process.argv[3] ?? "AqH29mZfQFgRpfwaPoTMWSKJ5kqauoc1FwVBRksZyQrt";
const timeoutMs = Number.parseInt(process.argv[4] ?? "30000", 10);

let requestId = 0;

async function rpc(method, params = []) {
  const response = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: ++requestId,
      method,
      params,
    }),
  });
  const body = await response.json();
  if (body.error) {
    throw new Error(`${method}: ${body.error.message}`);
  }
  return body.result;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

const startedAt = Date.now();
const deadline = startedAt + timeoutMs;

// This is the first half of Anchor's start_surfpool_validator readiness check.
while (true) {
  try {
    await rpc("getLatestBlockhash");
    break;
  } catch (error) {
    if (Date.now() >= deadline) {
      console.error(
        `SKIP: getLatestBlockhash did not become available within ${timeoutMs}ms: ${error}`,
      );
      process.exit(125);
    }
    await sleep(100);
  }
}

// This mirrors Anchor's second readiness check, including its treatment of an
// empty runbook list as "all runbooks completed".
while (true) {
  let info;
  try {
    info = await rpc("surfnet_getSurfnetInfo");
  } catch (error) {
    if (Date.now() >= deadline) {
      console.error(
        `SKIP: surfnet_getSurfnetInfo did not become available within ${timeoutMs}ms: ${error}`,
      );
      process.exit(125);
    }
    await sleep(100);
    continue;
  }

  const executions = info.value?.runbookExecutions ?? info.runbookExecutions ?? [];
  const startup = info.value?.startup ?? info.startup;
  if (executions.every((execution) => execution.completedAt != null)) {
    if (startup && startup.phase !== "ready") {
      console.error(
        `BAD: legacy readiness passed while Surfpool startup phase was ${startup.phase}`,
      );
      process.exit(1);
    }
    console.log(
      `Anchor readiness check returned after ${Date.now() - startedAt}ms with ${executions.length} runbook execution(s)` +
        (startup ? ` and startup phase ${startup.phase}` : ""),
    );
    break;
  }

  if (Date.now() >= deadline) {
    console.error("SKIP: deployment runbooks did not complete before timeout");
    process.exit(125);
  }
  await sleep(500);
}

const accountResponse = await rpc("getAccountInfo", [
  cloneAddress,
  { encoding: "base64", commitment: "confirmed" },
]);
const account = accountResponse.value ?? null;

if (account === null) {
  const readinessElapsedMs = Date.now() - startedAt;
  while (Date.now() < deadline) {
    await sleep(100);
    const eventualResponse = await rpc("getAccountInfo", [
      cloneAddress,
      { encoding: "base64", commitment: "confirmed" },
    ]);
    if (eventualResponse.value != null) {
      console.error(
        `BAD: Surfpool satisfied Anchor's readiness check after ${readinessElapsedMs}ms, ` +
          `but clone ${cloneAddress} was not installed until ${Date.now() - startedAt}ms`,
      );
      process.exit(1);
    }
  }
  console.error(
    `SKIP: clone ${cloneAddress} was still absent at readiness and never appeared; ` +
      "this revision did not exercise the intended delayed-clone race",
  );
  process.exit(125);
}

console.log(
  `GOOD: clone ${cloneAddress} was installed before Anchor's readiness check completed`,
);
