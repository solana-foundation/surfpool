use std::{collections::BTreeMap, sync::Arc};

use solana_account::Account;
use solana_keypair::Keypair;
use solana_program_option::COption;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_token_interface::state::{Account as TokenAccount, AccountState, Mint};
use surfpool_types::{HistoricalSessionOptions, HistoricalSimnetConfig};

use crate::{
    storage::StorageBackend,
    surfnet::{
        backend::historical::{HistoricalLaunch, launch},
        locker::SurfnetSvmLocker,
        svm::{SurfnetChannels, SurfnetSvm, SurfnetSvmConfig},
    },
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a staged SVM Replay backend and cached history (M59_* variables)"]
async fn historical_token_transfer_records_unindexed_accounts_and_mint() {
    let var = |name| std::env::var(name).unwrap();
    let config = HistoricalSimnetConfig::session(HistoricalSessionOptions {
        after: var("M59_AFTER"),
        program: Some(var("M59_SVM_REPLAY_BIN")),
        data_dir: Some(var("M59_HISTORICAL_DATA_DIR").into()),
        config: Some(var("M59_HISTORICAL_CONFIG").into()),
        ..Default::default()
    })
    .unwrap();
    let session = launch(
        &HistoricalLaunch {
            program: config.program.into(),
            args: config.args.into_iter().map(Into::into).collect(),
            envs: config
                .envs
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        },
        Arc::new(|line| eprintln!("{line}")),
        || false,
    )
    .unwrap();
    let fork = session.attach().unwrap();
    let (channels, _events, _geyser) = SurfnetChannels::new();
    let svm = SurfnetSvm::build_with(
        StorageBackend::open(&None, "token-receipt-test").unwrap(),
        None,
        channels,
        SurfnetSvmConfig {
            instruction_profiling_enabled: false,
            ..Default::default()
        },
        Some(fork.clone()),
    )
    .unwrap();
    let locker = SurfnetSvmLocker::new(svm);
    let payer = Keypair::new();
    let mint = Pubkey::new_unique();
    let source = Pubkey::new_unique();
    let destination = Pubkey::new_unique();
    let mut mint_data = vec![0; Mint::LEN];
    Mint::pack(
        Mint {
            mint_authority: COption::None,
            supply: 1_000,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        },
        &mut mint_data,
    )
    .unwrap();
    let token_image = |amount| {
        let mut data = vec![0; TokenAccount::LEN];
        TokenAccount::pack(
            TokenAccount {
                mint,
                owner: payer.pubkey(),
                amount,
                state: AccountState::Initialized,
                ..Default::default()
            },
            &mut data,
        )
        .unwrap();
        Account {
            lamports: 10_000_000,
            data,
            owner: spl_token_interface::id(),
            ..Default::default()
        }
    };
    // Backend images bypass Surfpool's registries, just as lazy historical reads do.
    fork.set_accounts(&[
        (
            payer.pubkey(),
            Account {
                lamports: 1_000_000_000,
                ..Default::default()
            },
        ),
        (
            mint,
            Account {
                lamports: 10_000_000,
                data: mint_data,
                owner: spl_token_interface::id(),
                ..Default::default()
            },
        ),
        (source, token_image(1_000)),
        (destination, token_image(0)),
    ])
    .unwrap();
    assert!(
        locker.with_svm_reader(|svm| svm.token_mints.get(&mint.to_string()).unwrap().is_none())
    );

    let transaction = Transaction::new_signed_with_payer(
        &[spl_token_interface::instruction::transfer(
            &spl_token_interface::id(),
            &source,
            &destination,
            &payer.pubkey(),
            &[],
            123,
        )
        .unwrap()],
        Some(&payer.pubkey()),
        &[&payer],
        fork.latest_blockhash().unwrap().0,
    );
    let signature = transaction.signatures[0];
    let (status, _updates) = crossbeam_channel::unbounded();
    let result = locker
        .fetch_all_tx_accounts_then_process_tx_returning_profile_res(
            &None,
            transaction.into(),
            &status,
            false,
            true,
            true,
        )
        .await;
    let amount = TokenAccount::unpack(&fork.get_account(&destination).unwrap().unwrap().data)
        .unwrap()
        .amount;
    assert_eq!(
        amount, 123,
        "the real engine must execute the token transfer"
    );
    result.expect("an executed transfer must retain its receipt");
    locker.with_svm_reader(|svm| {
        let record = svm
            .transactions
            .get(&signature.to_string())
            .unwrap()
            .unwrap()
            .as_processed()
            .unwrap()
            .0;
        assert!(record.meta.status.is_ok());
        let before = record.meta.pre_token_balances.unwrap();
        let after = record.meta.post_token_balances.unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 2);
        assert!(
            before
                .iter()
                .all(|balance| balance.ui_token_amount.decimals == 6)
        );
        assert!(
            after
                .iter()
                .all(|balance| balance.ui_token_amount.decimals == 6)
        );
        let amounts = |balances: Vec<solana_transaction_status::TransactionTokenBalance>| {
            balances
                .into_iter()
                .map(|balance| {
                    (
                        record.transaction.message.static_account_keys()
                            [balance.account_index as usize],
                        balance.ui_token_amount.amount,
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            amounts(before),
            BTreeMap::from([(source, "1000".into()), (destination, "0".into())])
        );
        assert_eq!(
            amounts(after),
            BTreeMap::from([
                (source, (1000 - 123).to_string()),
                (destination, "123".into())
            ])
        );
        assert_eq!(svm.transactions_queued_for_confirmation.len(), 1);
    });

    // A read-only zero-balance account remains usable when its mint is known absent.
    let orphan = Pubkey::new_unique();
    let receiver = Pubkey::new_unique();
    let receiver_lamports =
        locker.with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(0));
    fork.set_accounts(&[
        (orphan, token_image(0)),
        (
            receiver,
            Account {
                lamports: receiver_lamports,
                ..Default::default()
            },
        ),
    ])
    .unwrap();
    fork.delete_account(&mint).unwrap();
    locker.with_svm_writer(|svm| {
        svm.token_mints.take(&mint.to_string()).unwrap();
    });
    let mut instruction =
        solana_system_interface::instruction::transfer(&payer.pubkey(), &receiver, 1);
    instruction
        .accounts
        .push(solana_instruction::AccountMeta::new_readonly(orphan, false));
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[&payer],
        fork.latest_blockhash().unwrap().0,
    );
    let signature = transaction.signatures[0];
    locker
        .fetch_all_tx_accounts_then_process_tx_returning_profile_res(
            &None,
            transaction.into(),
            &status,
            false,
            true,
            true,
        )
        .await
        .expect("an absent mint must not reject an otherwise valid transaction");
    locker.with_svm_reader(|svm| {
        let record = svm
            .transactions
            .get(&signature.to_string())
            .unwrap()
            .unwrap()
            .as_processed()
            .unwrap()
            .0;
        assert!(record.meta.status.is_ok(), "{:?}", record.meta.status);
        assert!(record.meta.pre_token_balances.unwrap().is_empty());
        assert!(record.meta.post_token_balances.unwrap().is_empty());
        assert_eq!(svm.transactions_queued_for_confirmation.len(), 2);
    });
    assert_eq!(
        fork.get_account(&receiver).unwrap().unwrap().lamports,
        receiver_lamports + 1
    );
    session.terminate();
}
