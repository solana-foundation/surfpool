use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

use super::security_admission::{ipc_fixture, ipc_requests};
use crate::{
    rpc::full::{Full, SurfpoolFullRpc},
    scenarios::protocols::phoenix_eternal::v1::state_builder::Sandbox,
    surfnet::{backend::Backend, svm::SurfnetSvm},
    tests::helpers::TestSetup,
    types::{SurfnetTransactionStatus, TransactionWithStatusMeta},
};

#[test]
fn scenario_unsigned_execution_stays_on_its_disposable_branch() {
    let (mut live, _events, _geyser) = SurfnetSvm::default();
    let (fork, dir) = ipc_fixture(0);
    live.inner = Backend::historical(fork);
    let mut sandbox = Sandbox::new(&live).unwrap();
    let error = sandbox
        .view(&solana_instruction::Instruction::new_with_bytes(
            solana_pubkey::Pubkey::new_unique(),
            &[],
            vec![],
        ))
        .unwrap_err();
    assert_eq!(error.historical_code(), Some("TEST_RUNTIME_REFUSAL"));
    drop(sandbox);
    let requests = ipc_requests(&dir);
    assert_eq!(requests[0]["op"], "fork");
    assert_eq!(requests[0]["branch"], 0);
    assert!(requests[1..].iter().all(|request| request["branch"] == 1));
    let sends: Vec<_> = requests
        .iter()
        .filter(|request| request["op"]["command"]["method"] == "send")
        .collect();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["op"]["command"]["params"]["sigVerify"], false);
    assert_eq!(requests.last().unwrap()["op"], "close");
    assert!(
        requests
            .iter()
            .all(|request| request["op"].get("commit").is_none())
    );
}

#[test]
fn scenario_cannot_write_a_historical_clock_as_a_local_sysvar() {
    let (mut live, _events, _geyser) = SurfnetSvm::default();
    let (fork, dir) = ipc_fixture(0);
    live.inner = Backend::historical(fork);
    let mut sandbox = Sandbox::new(&live).unwrap();
    let before = ipc_requests(&dir);
    let error = sandbox.next_slot().unwrap_err();
    assert_eq!(
        error.historical_code(),
        Some("UNSUPPORTED_IN_HISTORICAL_MODE")
    );
    assert_eq!(ipc_requests(&dir), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_duplicate_preflight_does_not_use_local_lifetime_rules() {
    let setup = TestSetup::new(SurfpoolFullRpc);
    let (fork, dir) = ipc_fixture(0);
    let payer = Keypair::new();
    let transaction = Transaction::new_signed_with_payer(
        &[solana_system_interface::instruction::transfer(
            &payer.pubkey(),
            &solana_pubkey::Pubkey::new_unique(),
            1,
        )],
        Some(&payer.pubkey()),
        &[&payer],
        solana_hash::Hash::default(),
    );
    let versioned = solana_transaction::versioned::VersionedTransaction::from(transaction);
    setup.context.svm_locker.with_svm_writer(|svm| {
        svm.inner = Backend::historical(fork);
        svm.transactions
            .store(
                versioned.signatures[0].to_string(),
                SurfnetTransactionStatus::processed(
                    TransactionWithStatusMeta {
                        slot: 123,
                        transaction: versioned.clone(),
                        ..Default::default()
                    },
                    Default::default(),
                ),
            )
            .unwrap();
    });
    let error = setup
        .rpc
        .send_transaction(
            Some(setup.context),
            bs58::encode(wincode::serialize(&versioned).unwrap()).into_string(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.data.unwrap()["err"], "AlreadyProcessed");
    assert!(ipc_requests(&dir).is_empty());
}
