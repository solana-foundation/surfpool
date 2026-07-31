import { type ClientWithPayer, createKeyPairSignerFromBytes, extendClient, pipe, withCleanup } from '@solana/kit';
import { solanaLocalRpc, type SolanaRpcConfig } from '@solana/kit-plugin-rpc';
import type { SurfnetConfig } from '@solana/surfpool';

import { createSurfnetCheatcodesRpc } from './cheatcodes.js';

/**
 * Transaction planner/executor and RPC options forwarded to the standard
 * local-cluster Solana RPC plugin. URLs are excluded because they are
 * determined by the Surfpool instance (embedded) or given explicitly (attach).
 */
export type SurfpoolRpcOptions = Omit<SolanaRpcConfig<string>, 'rpcSubscriptionsUrl' | 'rpcUrl'>;

/** Configuration for {@link surfpool} in embedded mode (boots an in-process Surfnet). */
export type SurfpoolEmbeddedConfig = SurfpoolRpcOptions & {
    rpcSubscriptionsUrl?: never;
    rpcUrl?: never;
    /** Startup options forwarded verbatim to `Surfnet.startWithConfig()`. */
    surfnet?: SurfnetConfig;
};

/** Configuration for {@link surfpool} in attach mode (connects to a running Surfpool). */
export type SurfpoolAttachConfig = SurfpoolRpcOptions & {
    /**
     * The WebSocket URL of the running Surfpool instance. When omitted and
     * the `rpcUrl` has an explicit port, defaults to Surfpool's default
     * WebSocket port (8900, `--ws-port`) on the same host — Surfpool's
     * WebSocket port is independent of its HTTP port. For a `rpcUrl` without
     * a port (e.g. behind a proxy), only the protocol is swapped to
     * `ws`/`wss`. Set this explicitly when your setup differs.
     */
    rpcSubscriptionsUrl?: string;
    /** The HTTP RPC URL of a running Surfpool instance to attach to. */
    rpcUrl: string;
    surfnet?: never;
};

export type SurfpoolConfig = SurfpoolAttachConfig | SurfpoolEmbeddedConfig;

function surfpoolEmbedded(config: SurfpoolEmbeddedConfig = {}) {
    return async <T extends object>(client: T) => {
        const { rpcSubscriptionsUrl: _unusedWs, rpcUrl: _unusedRpc, surfnet: surfnetConfig, ...rpcOptions } = config;
        // Lazy imports keep the optional peers optional: the native module is
        // only needed in embedded mode, and the signer package is only needed
        // for the payer this mode installs.
        const [{ Surfnet }, { payer: payerPlugin }] = await Promise.all([
            import('@solana/surfpool'),
            import('@solana/kit-plugin-signer'),
        ]);
        const surfnet = surfnetConfig ? Surfnet.startWithConfig(surfnetConfig) : Surfnet.start();
        try {
            const payer = await createKeyPairSignerFromBytes(surfnet.payerSecretKey);

            const configuredClient = pipe(
                extendClient(client, {
                    cheatcodes: createSurfnetCheatcodesRpc(surfnet.rpcUrl, { headers: extractHeaders(rpcOptions) }),
                    rpcUrl: surfnet.rpcUrl,
                    surfnet,
                    wsUrl: surfnet.wsUrl,
                }),
                payerPlugin(payer),
                solanaLocalRpc({
                    ...rpcOptions,
                    rpcSubscriptionsUrl: surfnet.wsUrl,
                    rpcUrl: surfnet.rpcUrl,
                }),
            );

            // Disposing the client stops the in-process Surfnet so its servers
            // and ports are freed; recreating the client boots a fresh one.
            return withCleanup(configuredClient, () => surfnet.stop());
        } catch (error) {
            // If setup fails, the Surfnet handle never reaches the caller, so
            // it must be stopped here to free its servers and ports. `stop()`
            // can itself throw (shutdown timeout); the setup error is the one
            // worth surfacing.
            try {
                surfnet.stop();
            } catch {
                // Ignored in favor of the setup error.
            }
            throw error;
        }
    };
}

function surfpoolAttach(config: SurfpoolAttachConfig) {
    return <T extends ClientWithPayer>(client: T) => {
        const { rpcSubscriptionsUrl, rpcUrl, surfnet: _unusedSurfnet, ...rpcOptions } = config;
        const wsUrl = rpcSubscriptionsUrl ?? deriveSubscriptionsUrl(rpcUrl);

        return pipe(
            extendClient(client, {
                cheatcodes: createSurfnetCheatcodesRpc(rpcUrl, { headers: extractHeaders(rpcOptions) }),
                rpcUrl,
                wsUrl,
            }),
            solanaLocalRpc({
                ...rpcOptions,
                rpcSubscriptionsUrl: wsUrl,
                rpcUrl,
            }),
        );
    };
}

/**
 * Kit plugin for Surfpool. A drop-in replacement for `solanaLocalRpc()` or
 * `litesvm()` backed by a Surfpool Surfnet.
 *
 * **Embedded mode** (default): boots an in-process Surfnet on dynamic ports,
 * installs a `payer` signer for Surfnet's pre-funded payer account, and points
 * the local-cluster Solana RPC plugin (rpc, subscriptions, airdrop, planner,
 * executor, `sendTransactions`) at it. The native Surfnet handle is exposed as
 * `client.surfnet` for in-process helpers (`fundSol`, `deploy`, …), and a
 * typed cheatcodes RPC covering all `surfnet_*` methods as `client.cheatcodes`.
 * `identity` is left untouched — install your own with `.use(identity(...))`.
 * The client is {@link Disposable}: disposing it stops the Surfnet, so a
 * recreated client boots a fresh one.
 *
 * **Attach mode** (when `rpcUrl` is set): connects to an already-running
 * Surfpool instance (e.g. `surfpool start`) instead of booting one. No native
 * module is loaded, no `payer` is installed (the client must already have
 * one), and there is no `client.surfnet` handle.
 *
 * @example Embedded
 * ```ts
 * import { createClient } from '@solana/kit';
 * import { surfpool } from '@solana/surfpool/kit';
 *
 * const client = await createClient().use(surfpool());
 * await client.cheatcodes.timeTravel({ absoluteSlot: 1_000_000 }).send();
 * client.surfnet.fundSol(client.payer.address, 1_000_000_000);
 * ```
 *
 * @example Attach
 * ```ts
 * const client = await createClient()
 *     .use(payer(myPayer))
 *     .use(surfpool({ rpcUrl: 'http://127.0.0.1:8899' }));
 * ```
 */
export function surfpool(config?: SurfpoolEmbeddedConfig): ReturnType<typeof surfpoolEmbedded>;
export function surfpool(config: SurfpoolAttachConfig): ReturnType<typeof surfpoolAttach>;
export function surfpool(config: SurfpoolConfig = {}) {
    return isAttachConfig(config) ? surfpoolAttach(config) : surfpoolEmbedded(config);
}

function isAttachConfig(config: SurfpoolConfig): config is SurfpoolAttachConfig {
    return typeof config.rpcUrl === 'string';
}

function deriveSubscriptionsUrl(rpcUrl: string): string {
    // Surfpool serves WebSocket subscriptions on its own port (default 8900,
    // `--ws-port`), independent of the HTTP port. A protocol-swapped copy of
    // an explicit-port RPC URL would point at the HTTP port where nothing
    // speaks WebSocket, so explicit-port URLs map to port 8900 on the same
    // host; port-less URLs (e.g. behind a proxy) only swap the protocol.
    // WHATWG URL normalizes away scheme-default ports (`http://h:80`,
    // `https://h:443`), so those are detected from the raw string.
    const url = new URL(rpcUrl);
    const hasExplicitPort = url.port !== '' || /^[a-z+]+:\/\/[^/?#]+:\d+/i.test(rpcUrl);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    if (hasExplicitPort) {
        url.port = '8900';
    }
    const derived = url.toString();
    return !rpcUrl.endsWith('/') && url.pathname === '/' ? derived.replace(/\/$/, '') : derived;
}

function extractHeaders(rpcOptions: SurfpoolRpcOptions): Record<string, string> | undefined {
    return rpcOptions.rpcConfig?.headers as Record<string, string> | undefined;
}
