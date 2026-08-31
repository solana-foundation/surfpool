import type { Address, GetEpochInfoApi } from '@solana/kit';

import type {
    AccountSnapshot,
    AccountUpdate,
    CheatcodeControlConfig,
    ConfidentialBalanceKeys,
    DeriveConfidentialKeysResponse,
    ExportSnapshotConfig,
    GetConfidentialBalanceResponse,
    GetStreamedAccountsResponse,
    GetSurfnetInfoResponse,
    OfflineAccountConfig,
    ResetAccountConfig,
    RpcProfileResultConfig,
    Scenario,
    StreamAccountConfig,
    StreamAccountsEntry,
    SupplyUpdate,
    SurfnetCheatcodeMethod,
    TokenAccountUpdate,
    UiKeyedProfileResult,
} from '../generated/index.js';

// ── Handwritten wire types ──────────────────────────────────────────────────
// These few types are deliberately not generated: their custom Rust serde
// implementations (or foreign definitions) serialize to shapes that are
// expressed more precisely by hand. Each notes the Rust definition it mirrors.

/**
 * Clock state returned by `timeTravel`, `pauseClock`, and `resumeClock` —
 * the same shape as the standard `getEpochInfo` RPC response.
 *
 * Unlike the other cheatcodes, these three return this object bare — without
 * the `{ context, value }` envelope.
 */
export type EpochInfo = ReturnType<GetEpochInfoApi['getEpochInfo']>;

/**
 * Config for `timeTravel`. Mirrors `TimeTravelConfig` in
 * `crates/core/src/types.rs` (externally tagged, camelCase); its wire shape
 * is pinned by the `time_travel_config_wire_keys_are_stable` test next to
 * the `SurfnetCheatcodes` trait.
 */
export type TimeTravelConfig =
    | { absoluteEpoch: number | bigint }
    | { absoluteSlot: number | bigint }
    | { absoluteTimestamp: number | bigint };

/**
 * The string `"all"` to target every cheatcode, or a list of full method
 * names (e.g. `["surfnet_setAccount"]`) to target specific ones. Mirrors
 * `CheatcodeFilter` in `crates/types/src/types.rs` (untagged).
 */
export type CheatcodeFilter = 'all' | readonly string[];

/** Anchor IDL structure for `registerIdl` / `getActiveIdl`. */
export type AnchorIdl = Readonly<{
    accounts?: readonly unknown[];
    address: Address;
    constants?: readonly unknown[];
    errors?: readonly unknown[];
    events?: readonly unknown[];
    instructions: readonly unknown[];
    metadata: Readonly<{
        description?: string;
        name: string;
        spec: string;
        version: string;
    }>;
    types?: readonly unknown[];
}>;

/**
 * Entry returned by `getLocalSignatures`. Mirrors
 * `solana_rpc_client_api::response::RpcLogsResponse`.
 */
export type LocalSignatureEntry = Readonly<{
    /**
     * `null` on success, otherwise a `TransactionError` in its JSON-RPC
     * serialization (a string variant name or an object such as
     * `{ InstructionError: [...] }`).
     */
    err: unknown;
    logs: readonly string[];
    signature: string;
}>;

// ── Per-method API types ────────────────────────────────────────────────────

// Clock
export type SurfnetTimeTravelApi = {
    timeTravel(config?: TimeTravelConfig): EpochInfo;
};
export type SurfnetPauseClockApi = {
    pauseClock(): EpochInfo;
};
export type SurfnetResumeClockApi = {
    resumeClock(): EpochInfo;
};

// Cheatcode access control
export type SurfnetEnableCheatcodeApi = {
    enableCheatcode(filter: CheatcodeFilter): null;
};
export type SurfnetDisableCheatcodeApi = {
    disableCheatcode(filter: CheatcodeFilter, lockout?: CheatcodeControlConfig): null;
};

// Accounts
export type SurfnetSetAccountApi = {
    setAccount(pubkey: Address, update: AccountUpdate): null;
};
export type SurfnetSetTokenAccountApi = {
    setTokenAccount(owner: Address, mint: Address, update: TokenAccountUpdate, tokenProgram?: Address): null;
};
export type SurfnetGetConfidentialBalanceApi = {
    getConfidentialBalance(tokenAccount: Address, keys: ConfidentialBalanceKeys): GetConfidentialBalanceResponse;
};
export type SurfnetDeriveConfidentialKeysApi = {
    deriveConfidentialKeys(signature: string): DeriveConfidentialKeysResponse;
};
export type SurfnetResetAccountApi = {
    resetAccount(pubkey: Address, config?: ResetAccountConfig): null;
};
export type SurfnetOfflineAccountApi = {
    offlineAccount(pubkey: Address, config?: OfflineAccountConfig): null;
};
export type SurfnetStreamAccountApi = {
    streamAccount(pubkey: Address, config?: StreamAccountConfig): null;
};
export type SurfnetStreamAccountsApi = {
    streamAccounts(accounts: readonly StreamAccountsEntry[]): null;
};
export type SurfnetGetStreamedAccountsApi = {
    getStreamedAccounts(): GetStreamedAccountsResponse;
};

// Programs
export type SurfnetCloneProgramAccountApi = {
    cloneProgramAccount(sourceProgramId: Address, destinationProgramId: Address): null;
};
export type SurfnetSetProgramAuthorityApi = {
    setProgramAuthority(programId: Address, newAuthority?: Address): null;
};
export type SurfnetWriteProgramApi = {
    writeProgram(programId: Address, data: string, offset: number, authority?: Address): null;
};

// Profiling
export type SurfnetProfileTransactionApi = {
    profileTransaction(transactionData: string, tag?: string, config?: RpcProfileResultConfig): UiKeyedProfileResult;
};
export type SurfnetGetTransactionProfileApi = {
    getTransactionProfile(signatureOrUuid: string, config?: RpcProfileResultConfig): UiKeyedProfileResult | null;
};
export type SurfnetGetProfileResultsByTagApi = {
    getProfileResultsByTag(tag: string, config?: RpcProfileResultConfig): readonly UiKeyedProfileResult[] | null;
};

// IDL
export type SurfnetRegisterIdlApi = {
    registerIdl(idl: AnchorIdl, slot?: number | bigint): null;
};
export type SurfnetGetActiveIdlApi = {
    getActiveIdl(programId: Address, slot?: number | bigint): AnchorIdl | null;
};

// Network
export type SurfnetSetSupplyApi = {
    setSupply(update: SupplyUpdate): null;
};
export type SurfnetResetNetworkApi = {
    resetNetwork(): null;
};
export type SurfnetGetSurfnetInfoApi = {
    getSurfnetInfo(): GetSurfnetInfoResponse;
};
export type SurfnetExportSnapshotApi = {
    exportSnapshot(config?: ExportSnapshotConfig): Record<string, AccountSnapshot>;
};

// Scenario
export type SurfnetRegisterScenarioApi = {
    registerScenario(scenario: Scenario, slot?: number | bigint): null;
};

// Local
export type SurfnetGetLocalSignaturesApi = {
    getLocalSignatures(limit?: number | bigint): readonly LocalSignatureEntry[];
};

// ── Composed API ────────────────────────────────────────────────────────────

/**
 * All `surfnet_*` cheatcode methods, with the `surfnet_` prefix stripped
 * (it is re-added on the wire by the request transformer).
 */
export type SurfnetCheatcodesApi = SurfnetCloneProgramAccountApi &
    SurfnetDeriveConfidentialKeysApi &
    SurfnetDisableCheatcodeApi &
    SurfnetEnableCheatcodeApi &
    SurfnetExportSnapshotApi &
    SurfnetGetActiveIdlApi &
    SurfnetGetConfidentialBalanceApi &
    SurfnetGetLocalSignaturesApi &
    SurfnetGetProfileResultsByTagApi &
    SurfnetGetStreamedAccountsApi &
    SurfnetGetSurfnetInfoApi &
    SurfnetGetTransactionProfileApi &
    SurfnetOfflineAccountApi &
    SurfnetPauseClockApi &
    SurfnetProfileTransactionApi &
    SurfnetRegisterIdlApi &
    SurfnetRegisterScenarioApi &
    SurfnetResetAccountApi &
    SurfnetResetNetworkApi &
    SurfnetResumeClockApi &
    SurfnetSetAccountApi &
    SurfnetSetProgramAuthorityApi &
    SurfnetSetSupplyApi &
    SurfnetSetTokenAccountApi &
    SurfnetStreamAccountApi &
    SurfnetStreamAccountsApi &
    SurfnetTimeTravelApi &
    SurfnetWriteProgramApi;

// ── Manifest coverage assertion ─────────────────────────────────────────────
// Compile-time proof that SurfnetCheatcodesApi covers exactly the methods in
// the generated manifest. When a cheatcode is added or renamed in Rust, the
// regenerated manifest changes and this fails the build until the Api types
// above are updated.

type StripSurfnetPrefix<T> = T extends `surfnet_${infer TMethod}` ? TMethod : never;
type MutuallyAssignable<X, Y> = [X] extends [Y] ? ([Y] extends [X] ? true : false) : false;
type AssertTrue<T extends true> = T;
type _SurfnetApiCoversManifest = AssertTrue<
    MutuallyAssignable<keyof SurfnetCheatcodesApi, StripSurfnetPrefix<SurfnetCheatcodeMethod>>
>;
