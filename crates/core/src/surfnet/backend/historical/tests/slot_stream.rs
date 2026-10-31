use super::security_admission::{ipc_fixture, ipc_requests};
use crate::surfnet::{
    BlockHeader, GeyserEvent, GeyserSlotStatus,
    backend::Backend,
    svm::{SurfnetSvm, SurfnetSvmConfig},
};

#[test]
fn historical_startup_announces_the_stored_parent_without_advancing() {
    let (fork, dir) = ipc_fixture(0);
    let (mut svm, _events, geyser) = SurfnetSvm::new(SurfnetSvmConfig::default()).unwrap();
    svm.inner = Backend::historical(fork);
    svm.latest_epoch_info.absolute_slot = 100;
    svm.historical_open_block = Some((
        100,
        BlockHeader {
            hash: String::new(),
            previous_blockhash: String::new(),
            parent_slot: 70,
            block_time: 1,
            block_height: 1,
            signatures: vec![],
        },
    ));
    let before = (svm.updated_at, svm.blocks.count().unwrap());
    while geyser.try_recv().is_ok() {}

    svm.announce_open_slot().unwrap();

    assert!(matches!(
        geyser.try_recv().unwrap(),
        GeyserEvent::UpdateSlotStatus {
            slot: 100,
            parent: Some(70),
            status: GeyserSlotStatus::CreatedBank,
        }
    ));
    assert!(geyser.try_recv().is_err());
    assert_eq!(svm.get_latest_absolute_slot(), 100);
    assert_eq!((svm.updated_at, svm.blocks.count().unwrap()), before);
    assert!(ipc_requests(&dir).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_block_subscriptions_refuse_before_registration() {
    use std::sync::atomic::AtomicUsize;

    use jsonrpc_pubsub::typed::Subscriber;
    use solana_client::rpc_config::RpcBlockSubscribeFilter;

    use crate::rpc::{
        SurfpoolWebsocketMeta,
        ws::{Rpc, SurfpoolWsRpc},
    };

    let (fork, dir) = ipc_fixture(0);
    let setup = crate::tests::helpers::TestSetup::new(crate::rpc::minimal::SurfpoolMinimalRpc);
    setup
        .context
        .svm_locker
        .with_svm_writer(|svm| svm.inner = Backend::historical(fork));
    let rpc = SurfpoolWsRpc {
        uid: AtomicUsize::new(0),
        signature_subscription_map: Default::default(),
        account_subscription_map: Default::default(),
        program_subscription_map: Default::default(),
        slot_subscription_map: Default::default(),
        slots_updates_subscription_map: Default::default(),
        logs_subscription_map: Default::default(),
        block_subscription_map: Default::default(),
        snapshot_subscription_map: Default::default(),
        tokio_handle: tokio::runtime::Handle::current(),
    };
    let (subscriber, id, _notifications) = Subscriber::new_test("blockSubscribe");
    rpc.block_subscribe(
        Some(SurfpoolWebsocketMeta::new(setup.context.clone(), None)),
        subscriber,
        RpcBlockSubscribeFilter::All,
        None,
    );
    let error = id
        .await
        .unwrap()
        .expect_err("partial historical blocks must not be streamed");
    assert!(
        error.message.contains("UNSUPPORTED_IN_HISTORICAL_MODE"),
        "{error}"
    );
    assert!(rpc.block_subscription_map.read().unwrap().is_empty());
    assert!(
        setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.block_subscriptions.is_empty())
    );
    assert!(ipc_requests(&dir).is_empty());
}

#[test]
fn historical_startup_refuses_an_unproven_parent() {
    let (fork, dir) = ipc_fixture(0);
    let (mut svm, _events, geyser) = SurfnetSvm::new(SurfnetSvmConfig::default()).unwrap();
    svm.inner = Backend::historical(fork);
    while geyser.try_recv().is_ok() {}
    let error = svm.announce_open_slot().unwrap_err();
    assert_eq!(error.historical_code(), Some("SESSION_INTEGRITY"));
    assert!(geyser.try_recv().is_err());
    assert!(ipc_requests(&dir).is_empty());
}
