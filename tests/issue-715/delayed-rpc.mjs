import http from "node:http";

const port = Number.parseInt(process.argv[2] ?? "18898", 10);
const delayMs = Number.parseInt(process.argv[3] ?? "8000", 10);
const cloneAddress = "AqH29mZfQFgRpfwaPoTMWSKJ5kqauoc1FwVBRksZyQrt";

if (!Number.isInteger(port) || !Number.isInteger(delayMs)) {
  throw new Error("usage: node delayed-rpc.mjs <port> <delay-ms>");
}

const clonedAccount = {
  data: ["", "base64"],
  executable: false,
  lamports: 1_000_000,
  owner: "11111111111111111111111111111111",
  rentEpoch: 0,
  space: 0,
};

const server = http.createServer((request, response) => {
  let body = "";
  request.setEncoding("utf8");
  request.on("data", (chunk) => {
    body += chunk;
  });
  request.on("end", () => {
    let payload;
    try {
      payload = JSON.parse(body);
    } catch {
      response.writeHead(400).end();
      return;
    }

    const handle = () => {
      let result;
      switch (payload.method) {
        case "getMultipleAccounts": {
          const addresses = payload.params?.[0] ?? [];
          result = {
            context: { apiVersion: "2.0.0", slot: 1 },
            value: addresses.map((address) =>
              address === cloneAddress ? clonedAccount : null,
            ),
          };
          break;
        }
        default:
          response
            .writeHead(200, { "content-type": "application/json" })
            .end(
              JSON.stringify({
                jsonrpc: "2.0",
                id: payload.id,
                error: {
                  code: -32601,
                  message: `Method not found: ${payload.method}`,
                },
              }),
            );
          return;
      }

      response
        .writeHead(200, { "content-type": "application/json" })
        .end(JSON.stringify({ jsonrpc: "2.0", id: payload.id, result }));
    };

    if (payload.method === "getMultipleAccounts") {
      console.log(
        `delaying getMultipleAccounts by ${delayMs}ms for ${payload.params?.[0]?.join(",")}`,
      );
      setTimeout(handle, delayMs);
    } else {
      handle();
    }
  });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`delayed RPC listening on http://127.0.0.1:${port}`);
});
