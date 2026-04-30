import {
  ClockValue,
  DeployOptions as DeployOptionsInner,
  EpochInfoValue,
  KeypairInfo,
  ResetAccountOptions,
  SetTokenAccountUpdate,
  SimnetEventValue,
  SolAccountFunding,
  StreamAccountOptions,
  Surfnet as SurfnetInner,
  SurfnetConfig as SurfnetConfigInner,
} from "./internal";

export {
  ClockValue,
  EpochInfoValue,
  KeypairInfo,
  ResetAccountOptions,
  SetTokenAccountUpdate,
  SimnetEventValue,
  SolAccountFunding,
  StreamAccountOptions,
} from "./internal";

export type ByteArrayLike = Uint8Array | number[];

export type SurfnetConfig = Omit<SurfnetConfigInner, "payerSecretKey"> & {
  payerSecretKey?: ByteArrayLike;
};

export type DeployOptions = Omit<DeployOptionsInner, "soBytes"> & {
  soBytes?: ByteArrayLike;
};

/**
 * A running Surfpool instance with RPC/WS endpoints on dynamic ports.
 *
 * @example
 * ```ts
 * const surfnet = Surfnet.start();
 * console.log(surfnet.rpcUrl); // http://127.0.0.1:xxxxx
 *
 * surfnet.fundSol(address, 5_000_000_000); // 5 SOL
 * surfnet.fundToken(address, usdcMint, 1_000_000); // 1 USDC
 * ```
 */
export class Surfnet {
  private inner: SurfnetInner;

  private constructor(inner: SurfnetInner) {
    this.inner = inner;
  }

  /** Start a surfnet with default settings (offline, tx-mode blocks, 10 SOL payer). */
  static start(): Surfnet {
    return new Surfnet(SurfnetInner.start());
  }

  /** Start a surfnet with custom configuration. */
  static startWithConfig(config: SurfnetConfig): Surfnet {
    return new Surfnet(SurfnetInner.startWithConfig(normalizeConfig(config)));
  }

  /** The HTTP RPC URL (e.g. "http://127.0.0.1:12345"). */
  get rpcUrl(): string {
    return this.inner.rpcUrl;
  }

  /** The WebSocket URL (e.g. "ws://127.0.0.1:12346"). */
  get wsUrl(): string {
    return this.inner.wsUrl;
  }

  /** The pre-funded payer public key as a base58 string. */
  get payer(): string {
    return this.inner.payer;
  }

  /** The pre-funded payer secret key as a 64-byte Uint8Array. */
  get payerSecretKey(): Uint8Array {
    return Uint8Array.from(this.inner.payerSecretKey);
  }

  /** The unique identifier for this Surfnet instance. */
  get instanceId(): string {
    return this.inner.instanceId;
  }

  /**
   * Gracefully shut down the surfnet, closing the HTTP + WebSocket RPC
   * servers and freeing their ports. Blocks briefly while servers close.
   * Throws if shutdown is not confirmed within the timeout (the port may
   * still be bound). On success, subsequent calls are a no-op.
   */
  stop(): void {
    this.inner.stop();
  }

  /** Drain buffered simnet events into plain JS objects. */
  drainEvents(): SimnetEventValue[] {
    return this.inner.drainEvents();
  }

  /** Fund a SOL account with lamports. */
  fundSol(address: string, lamports: number): void {
    this.inner.fundSol(address, lamports);
  }

  /** Fund multiple SOL accounts with explicit lamport balances. */
  fundSolMany(accounts: SolAccountFunding[]): void {
    this.inner.fundSolMany(accounts);
  }

  /**
   * Fund a token account (creates the ATA if needed).
   * Uses spl_token program by default. Pass tokenProgram for Token-2022.
   */
  fundToken(
    owner: string,
    mint: string,
    amount: number,
    tokenProgram?: string,
  ): void {
    this.inner.fundToken(owner, mint, amount, tokenProgram ?? null);
  }

  /** Set the token balance for a wallet/mint pair. */
  setTokenBalance(
    owner: string,
    mint: string,
    amount: number,
    tokenProgram?: string,
  ): void {
    this.inner.setTokenBalance(owner, mint, amount, tokenProgram ?? null);
  }

  /** Set advanced token-account state for a wallet/mint pair. */
  setTokenAccount(
    owner: string,
    mint: string,
    update: SetTokenAccountUpdate,
    tokenProgram?: string,
  ): void {
    this.inner.setTokenAccount(owner, mint, update, tokenProgram ?? null);
  }

  /** Fund multiple wallets with the same token and amount. */
  fundTokenMany(
    owners: string[],
    mint: string,
    amount: number,
    tokenProgram?: string,
  ): void {
    this.inner.fundTokenMany(owners, mint, amount, tokenProgram ?? null);
  }

  /** Set arbitrary account data. */
  setAccount(
    address: string,
    lamports: number,
    data: Uint8Array,
    owner: string,
  ): void {
    this.inner.setAccount(address, lamports, Array.from(data), owner);
  }

  /** Reset a previously modified account to its upstream or absent state. */
  resetAccount(address: string, options?: ResetAccountOptions): void {
    this.inner.resetAccount(address, options ?? null);
  }

  /** Register an account for background streaming from the remote datasource. */
  streamAccount(address: string, options?: StreamAccountOptions): void {
    this.inner.streamAccount(address, options ?? null);
  }

  /** Move Surfnet time forward to an absolute epoch. */
  timeTravelToEpoch(epoch: number): EpochInfoValue {
    return this.inner.timeTravelToEpoch(epoch);
  }

  /** Move Surfnet time forward to an absolute slot. */
  timeTravelToSlot(slot: number): EpochInfoValue {
    return this.inner.timeTravelToSlot(slot);
  }

  /** Move Surfnet time forward to an absolute Unix timestamp in milliseconds. */
  timeTravelToTimestamp(timestamp: number): EpochInfoValue {
    return this.inner.timeTravelToTimestamp(timestamp);
  }

  /** Deploy a program by discovering local Anchor/Agave artifacts. */
  deployProgram(programName: string): string {
    return this.inner.deployProgram(programName);
  }

  /** Deploy a program from explicit bytes or an explicit `.so` path. */
  deploy(options: DeployOptions): string {
    return this.inner.deploy(normalizeDeployOptions(options));
  }

  /** Get the associated token address for a wallet/mint pair. */
  getAta(owner: string, mint: string, tokenProgram?: string): string {
    return this.inner.getAta(owner, mint, tokenProgram ?? null);
  }

  /** Generate a new random keypair. */
  static newKeypair(): KeypairInfo {
    return SurfnetInner.newKeypair();
  }
}

function normalizeConfig(config: SurfnetConfig): SurfnetConfigInner {
  return {
    ...config,
    payerSecretKey: config.payerSecretKey
      ? Array.from(config.payerSecretKey)
      : undefined,
  };
}

function normalizeDeployOptions(options: DeployOptions): DeployOptionsInner {
  return {
    ...options,
    soBytes: options.soBytes ? Array.from(options.soBytes) : undefined,
  };
}
