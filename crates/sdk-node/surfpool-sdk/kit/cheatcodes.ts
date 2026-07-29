import {
    createJsonRpcApi,
    createRpc,
    extendClient,
    type Rpc,
    type RpcResponse,
    type RpcResponseData,
    type RpcTransport,
} from '@solana/kit';

import { parseJsonWithBigInts, stringifyJsonWithBigInts } from './json.js';
import type { SurfnetCheatcodesApi } from './types/api.js';

export const DEFAULT_SURFNET_ENDPOINT = 'http://127.0.0.1:8899';

/** Options for the cheatcodes RPC factories. */
export type SurfnetCheatcodesConfig = {
    /** Extra HTTP headers to send with every cheatcode request (e.g. auth for a remote Surfpool). */
    headers?: Record<string, string>;
};

/**
 * Methods that return their result bare instead of wrapped in the standard
 * Solana RPC `{ context, value }` envelope.
 */
const BARE_RESULT_METHODS = new Set(['pauseClock', 'resumeClock', 'timeTravel']);

/**
 * A minimal HTTP transport with big-integer-safe JSON handling in both
 * directions. Kit's default transport applies its bigint codec only to the
 * standard Solana RPC methods, not to `surfnet_*` ones.
 */
function createSurfnetRpcTransport(url: string, headers: Record<string, string> = {}): RpcTransport {
    return async function transport<TResponse>({
        payload,
        signal,
    }: Readonly<{ payload: unknown; signal?: AbortSignal }>): Promise<TResponse> {
        const response = await fetch(url, {
            body: stringifyJsonWithBigInts(payload),
            headers: { ...headers, 'content-type': 'application/json' },
            method: 'POST',
            signal,
        });
        if (!response.ok) {
            throw new Error(`Surfnet RPC HTTP error (${response.status}): ${response.statusText}`);
        }
        return parseJsonWithBigInts(await response.text()) as TResponse;
    };
}

/**
 * Creates a Surfnet cheatcodes RPC client with clean method names.
 *
 * Method names have the `surfnet_` prefix stripped — e.g. `timeTravel()`
 * instead of `surfnet_timeTravel()`. The prefix is added back on the wire
 * via the request transformer. Surfnet wraps most cheatcode responses in
 * `{ context, value }`; the response transformer unwraps to the inner value.
 *
 * @param endpoint - The Surfpool HTTP RPC URL. Defaults to `http://127.0.0.1:8899`.
 *
 * @example
 * ```ts
 * const cheatcodes = createSurfnetCheatcodesRpc(surfnet.rpcUrl);
 * await cheatcodes.timeTravel({ absoluteSlot: 1_000_000 }).send();
 * ```
 */
export function createSurfnetCheatcodesRpc(
    endpoint: string = DEFAULT_SURFNET_ENDPOINT,
    config: SurfnetCheatcodesConfig = {},
): Rpc<SurfnetCheatcodesApi> {
    const api = createJsonRpcApi<SurfnetCheatcodesApi>({
        requestTransformer: request => ({
            ...request,
            methodName: `surfnet_${request.methodName}`,
        }),
        responseTransformer: (response: RpcResponse, request) => {
            const r = response as RpcResponseData<unknown>;
            if ('error' in r) {
                const detail =
                    r.error.data == null
                        ? ''
                        : ` — ${typeof r.error.data === 'string' ? r.error.data : stringifyJsonWithBigInts(r.error.data)}`;
                throw new Error(`Surfnet RPC error ${r.error.code}: ${r.error.message}${detail}`);
            }
            const result = r.result;
            const methodName = request.methodName.replace(/^surfnet_/, '');
            if (BARE_RESULT_METHODS.has(methodName)) {
                return result;
            }
            // Enveloped methods wrap their result in the standard Solana RPC
            // `{ context, value }` shape; only unwrap when it is present.
            if (result != null && typeof result === 'object' && 'context' in result && 'value' in result) {
                return result.value;
            }
            return result;
        },
    });
    const transport = createSurfnetRpcTransport(endpoint, config.headers);
    return createRpc({ api, transport });
}

/**
 * Kit plugin that installs a typed Surfnet cheatcodes RPC as `client.cheatcodes`.
 *
 * The endpoint is resolved from `config.url` if provided, otherwise from an
 * existing `client.rpcUrl` (as installed by the `surfpool()` plugin), and
 * falls back to `http://127.0.0.1:8899`.
 *
 * @example
 * ```ts
 * const client = createClient().use(surfnetCheatcodes({ url: 'http://127.0.0.1:8899' }));
 * await client.cheatcodes.pauseClock().send();
 * ```
 */
export function surfnetCheatcodes(config: SurfnetCheatcodesConfig & { url?: string } = {}) {
    return <T extends object>(client: T) => {
        const url = config.url ?? (client as { rpcUrl?: string }).rpcUrl ?? DEFAULT_SURFNET_ENDPOINT;
        return extendClient(client, { cheatcodes: createSurfnetCheatcodesRpc(url, { headers: config.headers }) });
    };
}
