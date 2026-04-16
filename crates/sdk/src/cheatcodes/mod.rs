use std::{
    env,
    path::{Path, PathBuf},
};

use solana_client::rpc_request::RpcRequest;
use solana_epoch_info::EpochInfo;
use solana_keypair::{EncodableKey, Keypair};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;

use crate::error::{SurfnetError, SurfnetResult};
pub mod builders;
use builders::{CheatcodeBuilder, DeployProgram};

/// Direct state manipulation helpers for a running Surfnet.
///
/// These bypass normal transaction flow to instantly set account state —
/// perfect for test setup (funding wallets, minting tokens, etc.).
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::SetAccount;
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
///
/// // Fund an account with 5 SOL
/// let alice: Pubkey = "...".parse().unwrap();
/// cheats.fund_sol(&alice, 5_000_000_000).unwrap();
///
/// // Fund a token account with 1000 USDC
/// let usdc_mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
/// cheats.fund_token(&alice, &usdc_mint, 1_000_000_000, None).unwrap();
///
/// // Or build a typed cheatcode request:
/// let custom = Pubkey::new_unique();
/// let owner = Pubkey::new_unique();
/// cheats
///     .execute(
///         SetAccount::new(custom)
///             .lamports(42)
///             .owner(owner)
///             .data(vec![1, 2, 3]),
///     )
///     .unwrap();
/// # }
/// ```
pub struct Cheatcodes<'a> {
    rpc_url: &'a str,
}

impl<'a> Cheatcodes<'a> {
    pub(crate) fn new(rpc_url: &'a str) -> Self {
        Self { rpc_url }
    }

    fn rpc_client(&self) -> RpcClient {
        RpcClient::new(self.rpc_url)
    }

    /// Set the SOL balance for an account in lamports.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let recipient = Pubkey::new_unique();
    ///
    /// cheats.fund_sol(&recipient, 1_000_000_000).unwrap();
    /// # }
    /// ```
    pub fn fund_sol(&self, address: &Pubkey, lamports: u64) -> SurfnetResult<()> {
        let params = serde_json::json!([
            address.to_string(),
            { "lamports": lamports }
        ]);
        self.call_cheatcode("surfnet_setAccount", params)
    }

    /// Set arbitrary account state for a single account.
    ///
    /// This helper updates lamports, owner, and raw account data in one RPC call.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let address = Pubkey::new_unique();
    /// let owner = Pubkey::new_unique();
    ///
    /// cheats.set_account(&address, 500, &[1, 2, 3], &owner).unwrap();
    /// # }
    /// ```
    pub fn set_account(
        &self,
        address: &Pubkey,
        lamports: u64,
        data: &[u8],
        owner: &Pubkey,
    ) -> SurfnetResult<()> {
        let params = serde_json::json!([
            address.to_string(),
            {
                "lamports": lamports,
                "data": hex::encode(data),
                "owner": owner.to_string()
            }
        ]);
        self.call_cheatcode("surfnet_setAccount", params)
    }

    /// Fund a token account (creates the ATA if needed).
    ///
    /// Uses `spl_token` program by default. Pass `token_program` to use Token-2022.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let owner = Pubkey::new_unique();
    /// let mint = Pubkey::new_unique();
    ///
    /// cheats.fund_token(&owner, &mint, 1_000, None).unwrap();
    /// # }
    /// ```
    pub fn fund_token(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        token_program: Option<&Pubkey>,
    ) -> SurfnetResult<()> {
        let program = token_program.copied().unwrap_or(spl_token_program_id());
        let params = serde_json::json!([
            owner.to_string(),
            mint.to_string(),
            { "amount": amount },
            program.to_string()
        ]);
        self.call_cheatcode("surfnet_setTokenAccount", params)
    }

    /// Set the token balance for a wallet/mint pair.
    ///
    /// This is an alias for [`Self::fund_token`].
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let owner = Pubkey::new_unique();
    /// let mint = Pubkey::new_unique();
    ///
    /// cheats.set_token_balance(&owner, &mint, 5_000, None).unwrap();
    /// # }
    /// ```
    pub fn set_token_balance(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        amount: u64,
        token_program: Option<&Pubkey>,
    ) -> SurfnetResult<()> {
        self.fund_token(owner, mint, amount, token_program)
    }

    /// Get the associated token address for a wallet/mint pair.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let owner = Pubkey::new_unique();
    /// let mint = Pubkey::new_unique();
    ///
    /// let ata = cheats.get_ata(&owner, &mint, None);
    /// println!("{ata}");
    /// # }
    /// ```
    pub fn get_ata(&self, owner: &Pubkey, mint: &Pubkey, token_program: Option<&Pubkey>) -> Pubkey {
        let program = token_program.copied().unwrap_or(spl_token_program_id());
        get_associated_token_address_with_program_id(owner, mint, &program)
    }

    /// Fund multiple accounts with SOL using repeated `surfnet_setAccount` calls.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let alice = Pubkey::new_unique();
    /// let bob = Pubkey::new_unique();
    ///
    /// cheats
    ///     .fund_sol_many(&[(&alice, 1_000_000), (&bob, 2_000_000)])
    ///     .unwrap();
    /// # }
    /// ```
    pub fn fund_sol_many(&self, accounts: &[(&Pubkey, u64)]) -> SurfnetResult<()> {
        for (address, lamports) in accounts {
            self.fund_sol(address, *lamports)?;
        }
        Ok(())
    }

    /// Fund multiple wallets with the same token and amount.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let alice = Pubkey::new_unique();
    /// let bob = Pubkey::new_unique();
    /// let mint = Pubkey::new_unique();
    ///
    /// cheats
    ///     .fund_token_many(&[&alice, &bob], &mint, 1_000, None)
    ///     .unwrap();
    /// # }
    /// ```
    pub fn fund_token_many(
        &self,
        owners: &[&Pubkey],
        mint: &Pubkey,
        amount: u64,
        token_program: Option<&Pubkey>,
    ) -> SurfnetResult<()> {
        for owner in owners {
            self.fund_token(owner, mint, amount, token_program)?;
        }
        Ok(())
    }

    /// Move Surfnet time forward to an absolute epoch.
    ///
    /// ```rust
    /// use surfpool_sdk::Surfnet;
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    ///
    /// let epoch_info = cheats.time_travel_to_epoch(10).unwrap();
    /// assert!(epoch_info.epoch >= 10);
    /// # }
    /// ```
    pub fn time_travel_to_epoch(&self, epoch: u64) -> SurfnetResult<EpochInfo> {
        self.time_travel(serde_json::json!([{ "absoluteEpoch": epoch }]))
    }

    /// Move Surfnet time forward to an absolute slot.
    ///
    /// ```rust
    /// use surfpool_sdk::Surfnet;
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    ///
    /// let epoch_info = cheats.time_travel_to_slot(1_000).unwrap();
    /// assert!(epoch_info.absolute_slot >= 1_000);
    /// # }
    /// ```
    pub fn time_travel_to_slot(&self, slot: u64) -> SurfnetResult<EpochInfo> {
        self.time_travel(serde_json::json!([{ "absoluteSlot": slot }]))
    }

    /// Move Surfnet time forward to an absolute Unix timestamp in milliseconds.
    ///
    /// ```rust
    /// use surfpool_sdk::Surfnet;
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    ///
    /// let epoch_info = cheats.time_travel_to_timestamp(1_700_000_000_000).unwrap();
    /// assert!(epoch_info.absolute_slot > 0);
    /// # }
    /// ```
    pub fn time_travel_to_timestamp(&self, timestamp: u64) -> SurfnetResult<EpochInfo> {
        self.time_travel(serde_json::json!([{ "absoluteTimestamp": timestamp }]))
    }

    /// Deploy a program from local workspace artifacts.
    ///
    /// This looks for:
    /// - `target/deploy/{program_name}.so`
    /// - `target/deploy/{program_name}-keypair.json`
    /// - `target/idl/{program_name}.json` (optional)
    ///
    /// If an IDL file exists, it is registered after the program bytes are written.
    ///
    /// ```rust
    /// use surfpool_sdk::Surfnet;
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    ///
    /// let program_id = cheats.deploy_program("my_program").unwrap();
    /// println!("{program_id}");
    /// # }
    /// ```
    pub fn deploy_program(&self, program_name: &str) -> SurfnetResult<Pubkey> {
        let target_dir = resolve_target_dir(program_name)?;
        let deploy_dir = target_dir.join("deploy");
        let idl_dir = target_dir.join("idl");
        let so_path = deploy_dir.join(format!("{program_name}.so"));
        let keypair_path = deploy_dir.join(format!("{program_name}-keypair.json"));
        let idl_path = idl_dir.join(format!("{program_name}.json"));

        let builder = DeployProgram::from_keypair_path(&keypair_path)?
            .so_path(so_path)
            .idl_path_if_exists(idl_path);

        self.deploy(builder)
    }

    /// Deploy a program described by a [`DeployProgram`] builder.
    ///
    /// This writes the program bytes with `surfnet_writeProgram` and, when present,
    /// registers the parsed IDL with `surfnet_registerIdl`.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    /// use surfpool_sdk::cheatcodes::builders::DeployProgram;
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let program_id = Pubkey::new_unique();
    ///
    /// let deployed_program = cheats
    ///     .deploy(
    ///         DeployProgram::new(program_id)
    ///             .so_path("target/deploy/my_program.so")
    ///             .idl_path("target/idl/my_program.json"),
    ///     )
    ///     .unwrap();
    ///
    /// assert_eq!(deployed_program, program_id);
    /// # }
    /// ```
    pub fn deploy(&self, builder: DeployProgram) -> SurfnetResult<Pubkey> {
        let program_id = builder.program_id();
        let program_bytes = builder.load_so_bytes()?;
        self.write_program(&program_id, &program_bytes)?;

        if let Some(mut idl) = builder.load_idl()? {
            idl.address = program_id.to_string();
            self.register_idl(&idl)?;
        }

        Ok(program_id)
    }

    /// Execute a typed cheatcode builder.
    ///
    /// ```rust
    /// use surfpool_sdk::{Pubkey, Surfnet};
    /// use surfpool_sdk::cheatcodes::builders::ResetAccount;
    ///
    /// # async fn example() {
    /// let surfnet = Surfnet::start().await.unwrap();
    /// let cheats = surfnet.cheatcodes();
    /// let address = Pubkey::new_unique();
    ///
    /// cheats.execute(ResetAccount::new(address)).unwrap();
    /// # }
    /// ```
    pub fn execute<B: CheatcodeBuilder>(&self, builder: B) -> SurfnetResult<()> {
        self.call_cheatcode(B::METHOD, builder.build())
    }

    /// Internal helper for `surfnet_timeTravel` requests that return [`EpochInfo`].
    fn time_travel(&self, params: serde_json::Value) -> SurfnetResult<EpochInfo> {
        let client = self.rpc_client();
        client
            .send::<EpochInfo>(
                RpcRequest::Custom {
                    method: "surfnet_timeTravel",
                },
                params,
            )
            .map_err(|e| SurfnetError::Cheatcode(format!("surfnet_timeTravel: {e}")))
    }

    fn write_program(&self, program_id: &Pubkey, data: &[u8]) -> SurfnetResult<()> {
        const PROGRAM_CHUNK_BYTES: usize = 15 * 1024 * 1024;

        for (index, chunk) in data.chunks(PROGRAM_CHUNK_BYTES).enumerate() {
            let offset = index * PROGRAM_CHUNK_BYTES;
            let params = serde_json::json!([program_id.to_string(), hex::encode(chunk), offset,]);
            self.call_cheatcode("surfnet_writeProgram", params)?;
        }

        Ok(())
    }

    fn register_idl(&self, idl: &surfpool_types::Idl) -> SurfnetResult<()> {
        let client = self.rpc_client();
        client
            .send::<serde_json::Value>(
                RpcRequest::Custom {
                    method: "surfnet_registerIdl",
                },
                serde_json::json!([idl]),
            )
            .map_err(|e| SurfnetError::Cheatcode(format!("surfnet_registerIdl: {e}")))?;
        Ok(())
    }

    /// Internal helper for cheatcodes that return `()`.
    fn call_cheatcode(&self, method: &'static str, params: serde_json::Value) -> SurfnetResult<()> {
        let client = self.rpc_client();
        client
            .send::<serde_json::Value>(RpcRequest::Custom { method }, params)
            .map_err(|e| SurfnetError::Cheatcode(format!("{method}: {e}")))?;
        Ok(())
    }
}

fn spl_token_program_id() -> Pubkey {
    // spl_token::id() = TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
}

fn read_keypair_pubkey(path: &Path) -> SurfnetResult<Pubkey> {
    Keypair::read_from_file(path)
        .map(|keypair| keypair.pubkey())
        .map_err(|e| {
            SurfnetError::Cheatcode(format!(
                "failed to read deploy keypair from {}: {e}",
                path.display()
            ))
        })
}

fn resolve_target_dir(program_name: &str) -> SurfnetResult<PathBuf> {
    if let Ok(explicit_target_dir) = env::var("CARGO_TARGET_DIR") {
        let target_dir = PathBuf::from(explicit_target_dir);
        if has_program_artifacts(&target_dir, program_name) {
            return Ok(target_dir);
        }
    }

    let current_dir = env::current_dir().map_err(|e| {
        SurfnetError::Cheatcode(format!("failed to resolve current working directory: {e}"))
    })?;

    for ancestor in current_dir.ancestors() {
        let target_dir = ancestor.join("target");
        if has_program_artifacts(&target_dir, program_name) {
            return Ok(target_dir);
        }
    }

    Err(SurfnetError::Cheatcode(format!(
        "failed to locate target/deploy artifacts for program `{program_name}` starting from {}",
        current_dir.display()
    )))
}

fn has_program_artifacts(target_dir: &Path, program_name: &str) -> bool {
    target_dir
        .join("deploy")
        .join(format!("{program_name}.so"))
        .exists()
        && target_dir
            .join("deploy")
            .join(format!("{program_name}-keypair.json"))
            .exists()
}

#[cfg(test)]
mod tests;
