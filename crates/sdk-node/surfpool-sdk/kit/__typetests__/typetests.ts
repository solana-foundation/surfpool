/**
 * Compile-only type tests for the kit entry. Never executed and excluded from
 * the emitting builds; checked by `npm run typecheck:kit`. Each
 * `@ts-expect-error` documents a misuse the types must keep rejecting.
 */
import { createClient, type KeyPairSigner } from '@solana/kit';

import { surfpool } from '../surfpool.js';

declare const payerSigner: KeyPairSigner;

// Embedded mode: no payer required up front; installs its own.
void (async () => {
    const client = await createClient().use(surfpool());
    client.surfnet.stop();
    void client.payer.address;
    void client.cheatcodes.pauseClock();
    void client.rpc.getSlot();
    void client.sendTransactions;
});

// Embedded mode accepts Surfnet startup options only under `surfnet:`.
void surfpool({ surfnet: { offline: true } });
// @ts-expect-error Surfnet options are not accepted at the top level.
void surfpool({ offline: true });
// @ts-expect-error unknown RPC option.
void surfpool({ maxConcurrencyy: 2 });

// Attach mode: requires an existing payer; no `surfnet` handle or config.
void (async () => {
    const attached = await createClient({ payer: payerSigner }).use(surfpool({ rpcUrl: 'http://127.0.0.1:8899' }));
    void attached.cheatcodes.getSurfnetInfo();
    void attached.rpc.getSlot();
    // @ts-expect-error attach mode has no native Surfnet handle.
    void attached.surfnet;
});
// @ts-expect-error attach mode requires the client to already have a payer.
void createClient().use(surfpool({ rpcUrl: 'http://127.0.0.1:8899' }));
// @ts-expect-error embedded startup options cannot be combined with attach mode.
void surfpool({ rpcUrl: 'http://127.0.0.1:8899', surfnet: { offline: true } });
