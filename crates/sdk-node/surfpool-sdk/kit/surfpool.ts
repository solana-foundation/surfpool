import {
    type Address,
    type ClientWithPayer,
    createKeyPairSignerFromBytes,
    extendClient,
    pipe,
    withCleanup,
} from '@solana/kit';
import { solanaLocalRpc, type SolanaRpcConfig } from '@solana/kit-plugin-rpc';
import type { SurfnetConfig } from '@solana/surfpool';

import { createSurfnetCheatcodesRpc } from './cheatcodes.js';

/** Lamports each `airdropAddresses` entry is credited with when no amount is given. */
const DEFAULT_AIRDROP_LAMPORTS = 10_000_000_000n;
const MAX_LAMPORTS = 2n ** 64n - 1n;

/** An address to fund, or anything carrying one (a signer, a PDA, an account). */
export type AirdropTarget = Address | { readonly address: Address };

/**
 * Transaction planner/executor and RPC options forwarded to the standard
 * local-cluster Solana RPC plugin. URLs are excluded because they are
 * determined by the Surfpool instance (embedded) or given explicitly (attach).
 */
export type SurfpoolRpcOptions = Omit<SolanaRpcConfig<string>, 'rpcSubscriptionsUrl' | 'rpcUrl'>;

/** Startup funding applied to both modes. */
type SurfpoolAirdropOptions = {
    /**
     * Addresses (or signers) credited with {@link SurfpoolAirdropOptions.airdropAmount}
     * lamports while the client is being composed. The amount is added to
     * whatever the address already holds, the way a real airdrop behaves.
     * Entries naming the same address are funded once.
     */
    airdropAddresses?: readonly AirdropTarget[];
    /**
     * Lamports to fund each entry of `airdropAddresses` with. Defaults to 10 SOL.
     * A `number` must be a safe integer; pass a `bigint` for amounts above 2^53.
     * Zero funds nothing.
     */
    airdropAmount?: bigint | number;
};

/** Configuration for {@link surfpool} in embedded mode (boots an in-process Surfnet). */
export type SurfpoolEmbeddedConfig = SurfpoolAirdropOptions &
    SurfpoolRpcOptions & {
        rpcSubscriptionsUrl?: never;
        rpcUrl?: never;
        /** Startup options forwarded verbatim to `Surfnet.startWithConfig()`. */
        surfnet?: SurfnetConfig;
    };

/** Configuration for {@link surfpool} in attach mode (connects to a running Surfpool). */
export type SurfpoolAttachConfig = SurfpoolAirdropOptions &
    SurfpoolRpcOptions & {
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

/** Attach-mode configuration that funds addresses, making the plugin asynchronous. */
export type SurfpoolAttachConfigWithAirdrop = SurfpoolAttachConfig & {
    airdropAddresses: readonly AirdropTarget[];
};

export type SurfpoolConfig = SurfpoolAttachConfig | SurfpoolEmbeddedConfig;

/**
 * A `number` above `Number.MAX_SAFE_INTEGER` has already lost precision by the
 * time it is read, and a fractional one is not a lamport amount at all. A
 * negative amount would debit the address instead of funding it, which is not
 * what an airdrop means. An amount beyond `u64::MAX` cannot be represented as a
 * lamport balance at all. All four are rejected instead of silently funding
 * something other than what was asked for.
 */
function toLamports(amount: bigint | number = DEFAULT_AIRDROP_LAMPORTS): bigint {
    if (typeof amount === 'number' && !Number.isSafeInteger(amount)) {
        throw new Error(`airdropAmount must be a safe integer or a bigint; received ${amount}`);
    }
    const lamports = BigInt(amount);
    if (lamports < 0n) {
        throw new Error(`airdropAmount must not be negative; received ${amount}`);
    }
    if (lamports > MAX_LAMPORTS) {
        throw new Error(`airdropAmount must not exceed ${MAX_LAMPORTS} lamports; received ${amount}`);
    }
    return lamports;
}

/**
 * Credits each target with `amount` lamports through the `setAccount`
 * cheatcode. The cheatcode writes an absolute balance, so the current balance
 * is read first and the amount added to it, matching what a real airdrop does
 * to an address that already holds lamports. Only the lamport balance is
 * written, so an existing account keeps its data and owner. A sum past
 * `u64::MAX` is not a representable balance and is rejected before it reaches
 * the cheatcode.
 */
async function fundAirdropAddresses(
    client: {
        cheatcodes: ReturnType<typeof createSurfnetCheatcodesRpc>;
        rpc: { getBalance: (address: Address) => { send: () => Promise<{ value: bigint }> } };
    },
    targets: readonly AirdropTarget[],
    amount: bigint,
): Promise<void> {
    if (amount === 0n) {
        return;
    }
    // Collapsing aliases to a set of addresses funds each exactly once.
    const addresses = new Set(targets.map(target => (typeof target === 'string' ? target : target.address)));
    await Promise.all(
        [...addresses].map(async address => {
            try {
                const { value: balance } = await client.rpc.getBalance(address).send();
                const lamports = balance + amount;
                if (lamports > MAX_LAMPORTS) {
                    throw new Error(
                        `balance ${balance} plus airdropAmount ${amount} exceeds the maximum lamport balance ${MAX_LAMPORTS}`,
                    );
                }
                await client.cheatcodes.setAccount(address, { lamports }).send();
            } catch (error) {
                throw new Error(`Failed to airdrop ${amount} lamports to ${address}`, { cause: error });
            }
        }),
    );
}

function surfpoolEmbedded(config: SurfpoolEmbeddedConfig = {}) {
    return async <T extends object>(client: T) => {
        const {
            airdropAddresses,
            airdropAmount,
            rpcSubscriptionsUrl: _unusedWs,
            rpcUrl: _unusedRpc,
            surfnet: surfnetConfig,
            ...rpcOptions
        } = config;
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

            if (airdropAddresses?.length) {
                await fundAirdropAddresses(configuredClient, airdropAddresses, toLamports(airdropAmount));
            }

            // Disposing the client stops the in-process Surfnet so its servers
            // and ports are freed; recreating the client boots a fresh one.
            if (typeof DisposableStack !== 'undefined') {
                return withCleanup(configuredClient, () => surfnet.stop());
            }
            // `withCleanup` needs the `DisposableStack` global (Node 24+). On
            // older runtimes register the disposer on `Symbol.dispose` directly,
            // chaining any existing one. Below Node 20 the symbol is absent and
            // the client is returned without a disposer.
            const disposeSymbol: symbol | undefined = (Symbol as { dispose?: symbol }).dispose;
            if (!disposeSymbol) {
                return configuredClient;
            }
            const existingDispose = (configuredClient as { [Symbol.dispose]?: () => void })[Symbol.dispose];
            return extendClient(configuredClient, {
                [Symbol.dispose]() {
                    existingDispose?.call(configuredClient);
                    surfnet.stop();
                },
            });
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
        const {
            airdropAddresses: _unusedAirdropAddresses,
            airdropAmount: _unusedAirdropAmount,
            rpcSubscriptionsUrl,
            rpcUrl,
            surfnet: _unusedSurfnet,
            ...rpcOptions
        } = config;
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

function surfpoolAttachFunded(config: SurfpoolAttachConfigWithAirdrop) {
    const attach = surfpoolAttach(config);
    return async <T extends ClientWithPayer>(client: T) => {
        const configuredClient = attach(client);
        await fundAirdropAddresses(configuredClient, config.airdropAddresses, toLamports(config.airdropAmount));
        return configuredClient;
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
 * one), and there is no `client.surfnet` handle. Because that payer is usually
 * unfunded on the running Surfnet, `airdropAddresses` credits it (and anything
 * else listed) with `airdropAmount` lamports as the client is composed; the
 * plugin then returns a promise, so `.use()` must be awaited.
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
 *     .use(surfpool({ airdropAddresses: [myPayer], rpcUrl: 'http://127.0.0.1:8899' }));
 * ```
 */
export function surfpool(config?: SurfpoolEmbeddedConfig): ReturnType<typeof surfpoolEmbedded>;
export function surfpool(config: SurfpoolAttachConfigWithAirdrop): ReturnType<typeof surfpoolAttachFunded>;
export function surfpool(
    config: SurfpoolAttachConfig & { airdropAddresses?: never },
): ReturnType<typeof surfpoolAttach>;
export function surfpool(config: SurfpoolConfig = {}) {
    if (!isAttachConfig(config)) {
        return surfpoolEmbedded(config);
    }
    return hasAirdropAddresses(config) ? surfpoolAttachFunded(config) : surfpoolAttach(config);
}

function isAttachConfig(config: SurfpoolConfig): config is SurfpoolAttachConfig {
    return typeof config.rpcUrl === 'string';
}

function hasAirdropAddresses(config: SurfpoolAttachConfig): config is SurfpoolAttachConfigWithAirdrop {
    return config.airdropAddresses !== undefined;
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
