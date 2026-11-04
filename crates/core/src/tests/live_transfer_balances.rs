//! Transfers against an already running surfnet, printing the pre/post balances that
//! `getTransaction` reports for each transfer.
//!
//! Start a surfnet, then run:
//!
//! ```sh
//! cargo test -p surfpool-core --lib live_transfer_balances -- --ignored --nocapture
//! ```
//!
//! Set `SURFPOOL_RPC_URL` to target an RPC other than `http://127.0.0.1:8899`.

use std::collections::HashMap;

use solana_client::{rpc_client::RpcClient, rpc_config::RpcTransactionConfig};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::Transaction;
use solana_transaction_status::UiTransactionEncoding;
use solana_transaction_status_client_types::{UiTransactionStatusMeta, UiTransactionTokenBalance};
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use spl_token_interface::{native_mint, state::Mint};

const DEFAULT_RPC_URL: &str = "http://127.0.0.1:8899";
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

struct Surfnet {
    rpc: RpcClient,
    payer: Keypair,
    labels: HashMap<Pubkey, String>,
}

impl Surfnet {
    fn connect() -> Self {
        let url = std::env::var("SURFPOOL_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.into());
        let rpc = RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed());
        rpc.get_version()
            .unwrap_or_else(|e| panic!("no surfnet answering at {url}: {e}"));
        let payer = Keypair::new();
        let airdrop = rpc
            .request_airdrop(&payer.pubkey(), 100 * LAMPORTS_PER_SOL)
            .expect("airdrop should be accepted");
        rpc.poll_for_signature(&airdrop)
            .expect("airdrop should confirm");
        let mut surfnet = Self {
            rpc,
            payer,
            labels: HashMap::new(),
        };
        surfnet.label(surfnet.payer.pubkey(), "payer (sender)");
        surfnet.label(solana_system_interface::program::ID, "System Program");
        surfnet.label(spl_token_interface::ID, "Token Program");
        surfnet.label(spl_token_2022_interface::ID, "Token-2022 Program");
        surfnet.label(native_mint::ID, "wSOL mint");
        surfnet.label(
            spl_associated_token_account_interface::program::ID,
            "ATA Program",
        );
        surfnet
    }

    fn label(&mut self, pubkey: Pubkey, label: impl Into<String>) {
        self.labels.insert(pubkey, label.into());
    }

    fn send(&self, instructions: &[Instruction], extra_signers: &[&Keypair]) -> Transaction {
        let mut signers = vec![&self.payer];
        signers.extend_from_slice(extra_signers);
        let tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&self.payer.pubkey()),
            &signers,
            self.rpc.get_latest_blockhash().unwrap(),
        );
        self.rpc
            .send_and_confirm_transaction(&tx)
            .expect("transaction should succeed");
        tx
    }

    /// Sends `instructions`, then prints and returns the status meta `getTransaction` reports.
    fn send_and_report(
        &self,
        title: &str,
        instructions: &[Instruction],
        extra_signers: &[&Keypair],
    ) -> (Transaction, UiTransactionStatusMeta) {
        let tx = self.send(instructions, extra_signers);
        let meta = self
            .rpc
            .get_transaction_with_config(
                &tx.signatures[0],
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(0),
                },
            )
            .expect("getTransaction should find the transfer")
            .transaction
            .meta
            .expect("the transfer should have a status meta");
        self.print(title, &tx, &meta);
        (tx, meta)
    }

    fn print(&self, title: &str, tx: &Transaction, meta: &UiTransactionStatusMeta) {
        let keys = &tx.message.account_keys;
        let name = |key: &Pubkey| {
            self.labels
                .get(key)
                .cloned()
                .unwrap_or_else(|| key.to_string())
        };
        println!("\n=== {title} ===");
        println!("signature: {}", tx.signatures[0]);
        println!("fee:       {} lamports", meta.fee);
        println!(
            "\n  {:<3} {:<26} {:>16} {:>16} {:>16}",
            "#", "account", "pre", "post", "delta"
        );
        for (index, key) in keys.iter().enumerate() {
            let (pre, post) = (meta.pre_balances[index], meta.post_balances[index]);
            println!(
                "  {:<3} {:<26} {:>16} {:>16} {:>+16}",
                index,
                name(key),
                pre,
                post,
                post as i128 - pre as i128
            );
        }
        let pre_tokens = token_balances(&meta.pre_token_balances);
        let post_tokens = token_balances(&meta.post_token_balances);
        if pre_tokens.is_empty() && post_tokens.is_empty() {
            println!("\n  (no token balances)");
            return;
        }
        println!(
            "\n  {:<3} {:<26} {:<20} {:>16} {:>16}",
            "#", "token account", "mint", "pre", "post"
        );
        let mut indexes: Vec<u8> = pre_tokens
            .keys()
            .chain(post_tokens.keys())
            .copied()
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        for index in indexes {
            let balance = post_tokens.get(&index).or(pre_tokens.get(&index)).unwrap();
            let amount = |balances: &HashMap<u8, &UiTransactionTokenBalance>| {
                balances.get(&index).map_or("-".to_string(), |b| {
                    b.ui_token_amount.ui_amount_string.clone()
                })
            };
            println!(
                "  {:<3} {:<26} {:<20} {:>16} {:>16}",
                index,
                name(&keys[index as usize]),
                name(&balance.mint.parse().unwrap()),
                amount(&pre_tokens),
                amount(&post_tokens),
            );
        }
    }

    /// Creates a mint owned by `token_program`, plus ATAs for the payer and `recipient`, and
    /// mints `supply` raw units to the payer's ATA. Returns the mint and the two ATAs.
    fn create_mint_with_atas(
        &mut self,
        token_program: Pubkey,
        name: &str,
        decimals: u8,
        recipient: &Pubkey,
        supply: u64,
    ) -> (Pubkey, Pubkey, Pubkey) {
        let mint = Keypair::new();
        let payer = self.payer.pubkey();
        let source =
            get_associated_token_address_with_program_id(&payer, &mint.pubkey(), &token_program);
        let destination =
            get_associated_token_address_with_program_id(recipient, &mint.pubkey(), &token_program);
        self.label(mint.pubkey(), format!("{name} mint"));
        self.label(source, format!("payer {name} ATA"));
        self.label(destination, format!("recipient {name} ATA"));
        let rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(Mint::LEN)
            .unwrap();
        self.send(
            &[
                system_instruction::create_account(
                    &payer,
                    &mint.pubkey(),
                    rent,
                    Mint::LEN as u64,
                    &token_program,
                ),
                spl_token_2022_interface::instruction::initialize_mint2(
                    &token_program,
                    &mint.pubkey(),
                    &payer,
                    None,
                    decimals,
                )
                .unwrap(),
                create_associated_token_account_idempotent(
                    &payer,
                    &payer,
                    &mint.pubkey(),
                    &token_program,
                ),
                create_associated_token_account_idempotent(
                    &payer,
                    recipient,
                    &mint.pubkey(),
                    &token_program,
                ),
                spl_token_2022_interface::instruction::mint_to_checked(
                    &token_program,
                    &mint.pubkey(),
                    &source,
                    &payer,
                    &[],
                    supply,
                    decimals,
                )
                .unwrap(),
            ],
            &[&mint],
        );
        (mint.pubkey(), source, destination)
    }
}

fn token_balances(
    balances: &solana_transaction_status_client_types::option_serializer::OptionSerializer<
        Vec<UiTransactionTokenBalance>,
    >,
) -> HashMap<u8, &UiTransactionTokenBalance> {
    Option::<&Vec<UiTransactionTokenBalance>>::from(balances.as_ref())
        .into_iter()
        .flatten()
        .map(|balance| (balance.account_index, balance))
        .collect()
}

fn index_of(tx: &Transaction, key: &Pubkey) -> usize {
    tx.message
        .account_keys
        .iter()
        .position(|k| k == key)
        .expect("the account should be in the transaction")
}

fn lamport_delta(meta: &UiTransactionStatusMeta, index: usize) -> i128 {
    meta.post_balances[index] as i128 - meta.pre_balances[index] as i128
}

fn raw_token_amount(
    balances: &solana_transaction_status_client_types::option_serializer::OptionSerializer<
        Vec<UiTransactionTokenBalance>,
    >,
    index: usize,
) -> u64 {
    token_balances(balances)
        .get(&(index as u8))
        .unwrap_or_else(|| panic!("account #{index} should have a token balance"))
        .ui_token_amount
        .amount
        .parse()
        .unwrap()
}

/// Asserts that a `transfer_checked` of `amount` moved tokens from `source` to `destination`.
fn assert_token_transfer(
    tx: &Transaction,
    meta: &UiTransactionStatusMeta,
    source: &Pubkey,
    destination: &Pubkey,
    amount: u64,
) {
    let (source, destination) = (index_of(tx, source), index_of(tx, destination));
    assert_eq!(
        raw_token_amount(&meta.pre_token_balances, source)
            - raw_token_amount(&meta.post_token_balances, source),
        amount
    );
    assert_eq!(
        raw_token_amount(&meta.post_token_balances, destination)
            - raw_token_amount(&meta.pre_token_balances, destination),
        amount
    );
    assert_eq!(lamport_delta(meta, 0), -(meta.fee as i128));
}

#[test]
#[ignore = "requires a running surfnet"]
fn live_transfer_balances() {
    let mut surfnet = Surfnet::connect();
    let payer = surfnet.payer.pubkey();
    let recipient = Keypair::new().pubkey();
    surfnet.label(recipient, "recipient");

    // SOL: a system transfer.
    let sol = LAMPORTS_PER_SOL;
    let (tx, meta) = surfnet.send_and_report(
        "SOL: system transfer of 1 SOL",
        &[system_instruction::transfer(&payer, &recipient, sol)],
        &[],
    );
    assert_eq!(
        lamport_delta(&meta, index_of(&tx, &payer)),
        -((sol + meta.fee) as i128)
    );
    assert_eq!(lamport_delta(&meta, index_of(&tx, &recipient)), sol as i128);

    // SPL Token: 250.0 of a 6-decimal mint.
    let (mint, source, destination) =
        surfnet.create_mint_with_atas(spl_token_interface::ID, "SPL", 6, &recipient, 1_000_000_000);
    let amount = 250_000_000;
    let (tx, meta) = surfnet.send_and_report(
        "SPL Token: transfer_checked of 250 SPL",
        &[spl_token_interface::instruction::transfer_checked(
            &spl_token_interface::ID,
            &source,
            &mint,
            &destination,
            &payer,
            &[],
            amount,
            6,
        )
        .unwrap()],
        &[],
    );
    assert_token_transfer(&tx, &meta, &source, &destination, amount);

    // Token-2022: 12.5 of a 9-decimal mint.
    let (mint, source, destination) = surfnet.create_mint_with_atas(
        spl_token_2022_interface::ID,
        "T22",
        9,
        &recipient,
        100_000_000_000,
    );
    let amount = 12_500_000_000;
    let (tx, meta) = surfnet.send_and_report(
        "Token-2022: transfer_checked of 12.5 T22",
        &[spl_token_2022_interface::instruction::transfer_checked(
            &spl_token_2022_interface::ID,
            &source,
            &mint,
            &destination,
            &payer,
            &[],
            amount,
            9,
        )
        .unwrap()],
        &[],
    );
    assert_token_transfer(&tx, &meta, &source, &destination, amount);

    // Native SOL token: wrap 2 SOL into the payer's wSOL ATA, then send 0.5 wSOL.
    let token = spl_token_interface::ID;
    let source = get_associated_token_address_with_program_id(&payer, &native_mint::ID, &token);
    let destination =
        get_associated_token_address_with_program_id(&recipient, &native_mint::ID, &token);
    surfnet.label(source, "payer wSOL ATA");
    surfnet.label(destination, "recipient wSOL ATA");
    let wrapped = 2 * LAMPORTS_PER_SOL;
    let (tx, meta) = surfnet.send_and_report(
        "Native SOL token: wrap 2 SOL",
        &[
            create_associated_token_account_idempotent(&payer, &payer, &native_mint::ID, &token),
            create_associated_token_account_idempotent(
                &payer,
                &recipient,
                &native_mint::ID,
                &token,
            ),
            system_instruction::transfer(&payer, &source, wrapped),
            spl_token_interface::instruction::sync_native(&token, &source).unwrap(),
        ],
        &[],
    );
    let source_index = index_of(&tx, &source);
    assert_eq!(
        raw_token_amount(&meta.post_token_balances, source_index),
        wrapped
    );

    let amount = LAMPORTS_PER_SOL / 2;
    let (tx, meta) = surfnet.send_and_report(
        "Native SOL token: transfer_checked of 0.5 wSOL",
        &[spl_token_interface::instruction::transfer_checked(
            &token,
            &source,
            &native_mint::ID,
            &destination,
            &payer,
            &[],
            amount,
            native_mint::DECIMALS,
        )
        .unwrap()],
        &[],
    );
    assert_token_transfer(&tx, &meta, &source, &destination, amount);
    // A wSOL transfer moves the lamports backing the tokens too.
    assert_eq!(
        lamport_delta(&meta, index_of(&tx, &source)),
        -(amount as i128)
    );
    assert_eq!(
        lamport_delta(&meta, index_of(&tx, &destination)),
        amount as i128
    );
}
