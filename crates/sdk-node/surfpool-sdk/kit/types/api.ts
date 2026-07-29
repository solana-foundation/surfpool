import type { Address, GetEpochInfoApi } from '@solana/kit';

import type {
    AccountSnapshot,
    AccountUpdate,
    CheatcodeControlConfig,
    CheatcodeFilter,
    ExportSnapshotConfig,
    GetStreamedAccountsResponse,
    GetSurfnetInfoResponse,
    OfflineAccountConfig,
    ResetAccountConfig,
    RpcProfileResultConfig,
    Scenario,
    StreamAccountConfig,
    StreamAccountsEntry,
    SupplyUpdate,
    TimeTravelConfig,
    TokenAccountUpdate,
    UiKeyedProfileResult,
} from './generated.js';

// ── RPC-only types (not defined in surfpool's Rust wire types) ──────────────

/**
 * Clock state returned by `timeTravel`, `pauseClock`, and `resumeClock` —
 * the same shape as the standard `getEpochInfo` RPC response.
 *
 * Unlike the other cheatcodes, these three return this object bare — without
 * the `{ context, value }` envelope.
 */
export type EpochInfo = ReturnType<GetEpochInfoApi['getEpochInfo']>;

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

/** Entry returned by `getLocalSignatures`. */
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
    SurfnetDisableCheatcodeApi &
    SurfnetEnableCheatcodeApi &
    SurfnetExportSnapshotApi &
    SurfnetGetActiveIdlApi &
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
