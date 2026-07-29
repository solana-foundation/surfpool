/**
 * Wire types for the `surfnet_*` cheatcode JSON-RPC methods.
 *
 * Hand-maintained mirror of the Rust types in `crates/types/src/types.rs` and
 * `crates/types/src/scenarios.rs`. Any change to those Rust types must be
 * reflected here until this file is replaced by generated bindings.
 *
 * Integer conventions: response integers are typed `bigint` because the
 * cheatcodes transport parses all JSON integers as `bigint` to avoid precision
 * loss above 2^53 (e.g. `rentEpoch` is u64::MAX on most mainnet accounts).
 * Request integers accept `number | bigint`; both serialize to plain JSON
 * integers.
 */

// ── Request payloads ────────────────────────────────────────────────────────

export type AccountUpdate = {
    /** Hex-encoded account data. */
    data?: string;
    /** Whether this account's data contains a loaded program (and is now read-only). */
    executable?: boolean;
    /** Sets the lamports in the account. */
    lamports?: number | bigint;
    /** Sets the program that owns this account. If executable, the program that loads this account. */
    owner?: string;
    /** Sets the epoch at which this account will next owe rent. */
    rentEpoch?: number | bigint;
};

/**
 * A base58 pubkey string, or the literal string `"null"` to clear the field.
 * Omitting the field leaves it unchanged.
 */
export type SetSomeAccount = string;

/**
 * Configures the Token-2022 confidential-transfer extension on a token
 * account created via `setTokenAccount`.
 */
export type ConfidentialTransferAccountUpdate = {
    /** The owner's AES secret key (base58 or base64, 16 bytes). Required by the server. */
    aesKey?: string;
    /** Whether the account accepts incoming confidential credits (default true). */
    allowConfidentialCredits?: boolean;
    /** Whether the base account accepts incoming non-confidential credits (default true). */
    allowNonConfidentialCredits?: boolean;
    /** The confidential available balance to set (default 0). */
    amount?: number | bigint;
    /** Whether the account is approved for confidential transfers (default true). */
    approved?: boolean;
    /** The owner's ElGamal public key (base58 or base64, 32 bytes). */
    elgamalPubkey: string;
    /** The maximum pending-balance credit counter (default 65536). */
    maximumPendingBalanceCreditCounter?: number | bigint;
};

export type TokenAccountUpdate = {
    /** Sets the amount of the token in the account data. */
    amount?: number | bigint;
    /** Sets the close authority of the token account. */
    closeAuthority?: SetSomeAccount;
    /** Configures the Token-2022 confidential-transfer extension (Token-2022 only). */
    confidential?: ConfidentialTransferAccountUpdate;
    /** Sets the delegate of the token account. */
    delegate?: SetSomeAccount;
    /** Sets the amount authorized to the delegate. */
    delegatedAmount?: number | bigint;
    /** Sets the state of the token account. */
    state?: string;
};

/** Note: this type serializes with snake_case keys on the wire. */
export type SupplyUpdate = {
    circulating?: number | bigint;
    non_circulating?: number | bigint;
    non_circulating_accounts?: readonly string[];
    total?: number | bigint;
};

export type TimeTravelConfig =
    | { absoluteEpoch: number | bigint }
    | { absoluteSlot: number | bigint }
    | { absoluteTimestamp: number | bigint };

/**
 * The string `"all"` to target every cheatcode, or a list of full method
 * names (e.g. `["surfnet_setAccount"]`) to target specific ones.
 */
export type CheatcodeFilter = 'all' | readonly string[];

export type CheatcodeControlConfig = {
    /** When true, allows disabling even `enableCheatcode`/`disableCheatcode` themselves. */
    lockout?: boolean;
};

export type UiAccountEncoding = 'base58' | 'base64' | 'base64+zstd' | 'binary' | 'jsonParsed';

export type RpcProfileDepth = 'instruction' | 'transaction';

export type RpcProfileResultConfig = {
    depth?: RpcProfileDepth;
    encoding?: UiAccountEncoding;
};

export type ResetAccountConfig = {
    includeOwnedAccounts?: boolean;
};

export type StreamAccountConfig = {
    includeOwnedAccounts?: boolean;
};

export type StreamAccountsEntry = {
    includeOwnedAccounts?: boolean;
    pubkey: string;
};

export type OfflineAccountConfig = {
    includeOwnedAccounts?: boolean;
};

export type ExportSnapshotScope = 'network' | { preTransaction: string };

export type ExportSnapshotFilter = {
    excludeAccounts?: readonly string[];
    /** When true, omit accounts whose pubkey is a known agave feature gate. */
    excludeFeatureGates?: boolean;
    /** When true, omit accounts owned by the sysvar program. */
    excludeSysvars?: boolean;
    includeAccounts?: readonly string[];
    includeProgramAccounts?: boolean;
};

export type ExportSnapshotConfig = {
    filter?: ExportSnapshotFilter;
    includeParsedAccounts?: boolean;
    scope: ExportSnapshotScope;
};

/** A concrete instance of an override template with specific values. */
export type OverrideInstance = {
    /** Whether this override is active. */
    enabled: boolean;
    /** If true, fetches fresh account data from the datasource before applying the override. */
    fetchBeforeUse?: boolean;
    /** Unique identifier for this instance (UUID v4). */
    id: string;
    /** Optional human-readable label for this instance. */
    label?: string;
    /** Slot offset from scenario registration (each slot is ~400ms). */
    scenarioRelativeSlot: number | bigint;
    /** Template ID from the override-template registry. */
    templateId: string;
    /** Values for the template properties. */
    values: Record<string, unknown>;
};

/** A scenario containing a timeline of overrides. */
export type Scenario = {
    description: string;
    /** Unique identifier for the scenario (UUID v4). */
    id: string;
    name: string;
    overrides: readonly OverrideInstance[];
    tags: readonly string[];
};

// ── Response payloads ───────────────────────────────────────────────────────

export type ParsedAccount = {
    parsed: unknown;
    program: string;
    space: bigint;
};

export type UiAccountData = string | ParsedAccount | readonly [string, UiAccountEncoding];

export type UiAccount = {
    data: UiAccountData;
    executable: boolean;
    lamports: bigint;
    owner: string;
    rentEpoch: bigint;
    space?: bigint;
};

export type UiAccountChange =
    | { data: readonly [UiAccount, UiAccount]; type: 'update' }
    | { data: UiAccount; type: 'create' }
    | { data: UiAccount; type: 'delete' }
    | { data: UiAccount | null; type: 'unchanged' };

export type UiAccountProfileState = { type: 'readonly' } | { accountChange: UiAccountChange; type: 'writable' };

export type UiProfileResult = {
    accountStates: Record<string, UiAccountProfileState>;
    computeUnitsConsumed: bigint;
    errorMessage: string | null;
    logMessages: readonly string[] | null;
};

export type UiKeyedProfileResult = {
    instructionProfiles?: readonly UiProfileResult[];
    /** The transaction signature or profile UUID this result is keyed by. */
    key: string;
    readonlyAccountStates: Record<string, UiAccount>;
    slot: bigint;
    transactionProfile: UiProfileResult;
};

export type AccountSnapshot = {
    /** Base64-encoded account data. */
    data: string;
    executable: boolean;
    lamports: bigint;
    owner: string;
    /** Parsed account data if available. */
    parsedData: ParsedAccount | null;
    rentEpoch: bigint;
};

export type StreamedAccountInfo = {
    includeOwnedAccounts: boolean;
    pubkey: string;
};

export type GetStreamedAccountsResponse = {
    accounts: readonly StreamedAccountInfo[];
};

export type RunbookExecutionStatusReport = {
    completedAt: bigint | null;
    errors: readonly string[] | null;
    runbookId: string;
    startedAt: bigint;
};

export type GetSurfnetInfoResponse = {
    runbookExecutions: readonly RunbookExecutionStatusReport[];
};
