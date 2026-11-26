//! The TPU QUIC ingress of a surfnet.
//!
//! Clients that send straight to the leader (the `tpu-client-next` transport
//! behind `solana program deploy`, for one) look the leader's QUIC socket up
//! with `getClusterNodes`, open one unidirectional stream per transaction
//! and never hear back. This server is that socket: every transaction it
//! receives is queued on the runloop like a `sendTransaction` that skips
//! preflight.

use std::{
    io,
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::Duration,
};

use crossbeam_channel::{Sender, bounded};
use quinn::{
    Endpoint, EndpointConfig, IdleTimeout, Incoming, ServerConfig, TokioRuntime, VarInt,
    crypto::rustls::QuicServerConfig,
};
use solana_keypair::Keypair;
use solana_message::v1::MAX_TRANSACTION_SIZE;
use solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder};
use solana_transaction::versioned::{
    VersionedTransaction, sanitized::SanitizedVersionedTransaction,
};
use surfpool_types::SimnetCommand;

use crate::{rpc::utils::wire_size_limit, surfnet::locker::SurfnetSvmLocker};

/// The protocol TPU clients negotiate during the TLS handshake.
const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";
/// How long a connection may stay silent, as on an Agave validator. Clients
/// keep theirs alive with pings well inside this window.
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Transactions a single connection may have in flight at once.
const MAX_CONCURRENT_STREAMS: u32 = 512;
/// Transactions, from any ingress, that may wait in or run on the runloop
/// before the TPU drops new arrivals.
const MAX_PENDING_TRANSACTIONS: usize = 10_000;

/// A running TPU QUIC server.
pub(crate) struct TpuQuicServer {
    endpoint: Endpoint,
    local_addr: SocketAddr,
}

impl TpuQuicServer {
    /// The address the server listens on.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops the server and drops its open connections.
    pub(crate) fn close(&self) {
        self.endpoint.close(VarInt::from_u32(0), b"shutdown");
    }
}

/// Starts the TPU QUIC server on `addr` and returns once it accepts
/// connections. With port `0` the OS picks the port, which
/// [`TpuQuicServer::local_addr`] reports.
pub(crate) fn start_tpu_quic_runloop(
    addr: SocketAddr,
    svm_locker: SurfnetSvmLocker,
    simnet_commands_tx: Sender<SimnetCommand>,
) -> io::Result<TpuQuicServer> {
    let socket = UdpSocket::bind(addr)?;
    socket.set_nonblocking(true)?;
    let local_addr = socket.local_addr()?;
    let server_config = server_config().map_err(io::Error::other)?;
    let ingress = TpuIngress {
        svm_locker,
        simnet_commands_tx,
    };

    let (endpoint_tx, endpoint_rx) = bounded::<io::Result<Endpoint>>(1);
    hiro_system_kit::thread_named("TPU QUIC Handler").spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = endpoint_tx.send(Err(e));
                return;
            }
        };
        runtime.block_on(async move {
            let endpoint = match Endpoint::new(
                EndpointConfig::default(),
                Some(server_config),
                socket,
                Arc::new(TokioRuntime),
            ) {
                Ok(endpoint) => endpoint,
                Err(e) => {
                    let _ = endpoint_tx.send(Err(e));
                    return;
                }
            };
            let _ = endpoint_tx.send(Ok(endpoint.clone()));
            while let Some(incoming) = endpoint.accept().await {
                tokio::spawn(handle_connection(incoming, ingress.clone()));
            }
        });
    })?;

    let endpoint = endpoint_rx
        .recv()
        .map_err(|_| io::Error::other("TPU QUIC Handler thread exited during startup"))??;
    Ok(TpuQuicServer {
        endpoint,
        local_addr,
    })
}

fn server_config() -> Result<ServerConfig, String> {
    // TPU clients don't verify the leader's certificate, so a throwaway identity is enough.
    let (cert, key) = new_dummy_x509_certificate(&Keypair::new());
    let mut tls_config = tls_server_config_builder()
        .with_single_cert(vec![cert], key)
        .map_err(|e| e.to_string())?;
    tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];
    let crypto = QuicServerConfig::try_from(tls_config).map_err(|e| e.to_string())?;

    let mut server_config = ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_uni_streams(MAX_CONCURRENT_STREAMS.into());
    // A stream carries exactly one transaction.
    transport.stream_receive_window(VarInt::from_u32(MAX_TRANSACTION_SIZE as u32));
    transport.max_concurrent_bidi_streams(0u32.into());
    transport.datagram_receive_buffer_size(None);
    transport.max_idle_timeout(Some(
        IdleTimeout::try_from(MAX_IDLE_TIMEOUT).map_err(|e| e.to_string())?,
    ));
    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
}

async fn handle_connection(incoming: Incoming, ingress: TpuIngress) {
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(e) => {
            debug!("TPU QUIC handshake failed: {e}");
            return;
        }
    };
    while let Ok(mut stream) = connection.accept_uni().await {
        let ingress = ingress.clone();
        tokio::spawn(async move {
            match stream.read_to_end(MAX_TRANSACTION_SIZE).await {
                Ok(wire_transaction) => ingress.submit(&wire_transaction),
                Err(e) => debug!("Dropping TPU transaction: {e}"),
            }
        });
    }
}

/// Hands transactions received over TPU to the runloop.
#[derive(Clone)]
struct TpuIngress {
    svm_locker: SurfnetSvmLocker,
    simnet_commands_tx: Sender<SimnetCommand>,
}

impl TpuIngress {
    /// Queues a wire transaction for execution. A transaction that doesn't
    /// decode or sanitize is dropped, as a validator would, and so is any
    /// transaction that arrives while the runloop is saturated.
    fn submit(&self, wire_transaction: &[u8]) {
        let transaction = match decode_wire_transaction(wire_transaction) {
            Ok(transaction) => transaction,
            Err(e) => {
                debug!("Dropping TPU transaction: {e}");
                return;
            }
        };
        let signature = transaction.signatures()[0];
        // TPU senders learn the outcome from signature statuses, so nothing reads this channel.
        let (status_tx, _) = bounded(1);
        // A busy leader drops what it can't buffer, and TPU senders retry on their own.
        if !self
            .svm_locker
            .try_mark_transaction_pending(signature, MAX_PENDING_TRANSACTIONS)
        {
            debug!("Dropping TPU transaction {signature}: the runloop is saturated");
            return;
        }
        if self
            .simnet_commands_tx
            .send(SimnetCommand::ProcessTransaction(
                None,
                transaction,
                status_tx,
                true,
                None,
            ))
            .is_err()
        {
            self.svm_locker.mark_transaction_complete(&signature);
        }
    }
}

fn decode_wire_transaction(wire: &[u8]) -> Result<SanitizedVersionedTransaction, String> {
    let size_limit = wire_size_limit(wire);
    if wire.len() > size_limit {
        return Err(format!(
            "transaction too large: {} bytes (max: {size_limit} bytes)",
            wire.len()
        ));
    }
    let transaction: VersionedTransaction =
        wincode::deserialize(wire).map_err(|e| format!("failed to deserialize: {e}"))?;
    SanitizedVersionedTransaction::try_new(transaction)
        .map_err(|e| format!("invalid transaction: {e}"))
}
