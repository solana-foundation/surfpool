//! Prometheus metrics for Surfpool
//!
//! Feature `prometheus` enables a `/metrics` HTTP endpoint

mod instrumented {
    use std::sync::{Once, OnceLock};

    use opentelemetry::{
        KeyValue,
        metrics::{Counter, Histogram, Meter, MeterProvider},
    };
    use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider};
    use prometheus::Encoder;

    static INIT: Once = Once::new();
    static METRICS: OnceLock<SurfpoolMetrics> = OnceLock::new();
    static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

    #[derive(Debug)]
    pub struct SurfpoolMetrics {
        // Transaction success/failure rate
        tx_count: Counter<u64>,

        // Transaction processing latency
        tx_latency_ms: Histogram<u64>,

        // RPC request rate by method
        rpc_request_count: Counter<u64>,

        // RPC request latency by method
        rpc_latency_ms: Histogram<u64>,

        // Remote account fetch latency
        remote_fetch_latency_ms: Histogram<u64>,
    }

    impl SurfpoolMetrics {
        fn new(meter: Meter) -> Self {
            Self {
                tx_count: meter
                    .u64_counter("surfpool_transactions")
                    .with_description("Total transactions by status (success/failure)")
                    .build(),

                tx_latency_ms: meter
                    .u64_histogram("surfpool_transaction_processing_ms")
                    .with_description("Transaction processing latency in milliseconds")
                    .build(),

                rpc_request_count: meter
                    .u64_counter("surfpool_rpc_requests")
                    .with_description("RPC requests by method")
                    .build(),

                rpc_latency_ms: meter
                    .u64_histogram("surfpool_rpc_latency_ms")
                    .with_description("RPC request latency in milliseconds by method")
                    .build(),

                remote_fetch_latency_ms: meter
                    .u64_histogram("surfpool_remote_fetch_latency_ms")
                    .with_description("Remote account fetch latency from mainnet in milliseconds")
                    .build(),
            }
        }

        /// Called in send_transaction() — records whether the transaction was accepted into
        /// the queue (not its execution outcome) + latency
        pub fn record_transaction(&self, success: bool, latency_ms: u64) {
            let status = if success { "success" } else { "failure" };
            self.tx_count.add(1, &[KeyValue::new("status", status)]);
            self.tx_latency_ms.record(latency_ms, &[]);
        }

        /// Called in RPC handlers — records which method was called + how long it took
        pub fn record_rpc_request(&self, method: &str, latency_ms: u64) {
            let attrs = &[KeyValue::new("method", method.to_string())];
            self.rpc_request_count.add(1, attrs);
            self.rpc_latency_ms.record(latency_ms, attrs);
        }

        /// Called when fetching accounts from mainnet (remote cloning)
        pub fn record_remote_fetch(&self, latency_ms: u64) {
            self.remote_fetch_latency_ms.record(latency_ms, &[]);
        }
    }

    pub fn init_prometheus(
        service_name: &str,
        bind_addr: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let service_name_owned = service_name.to_string();
        let bind_addr_owned = bind_addr.to_string();
        let mut result = Ok(());

        INIT.call_once(|| {
            let registry = prometheus::Registry::new();
            let exporter = match opentelemetry_prometheus::exporter()
                .with_registry(registry.clone())
                .build()
            {
                Ok(exp) => exp,
                Err(e) => {
                    result = Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                    return;
                }
            };

            let resource = Resource::builder()
                .with_attributes(vec![KeyValue::new("service.name", service_name_owned)])
                .build();

            let provider = SdkMeterProvider::builder()
                .with_resource(resource)
                .with_reader(exporter)
                .build();

            let meter = provider.meter("surfpool-core");
            let metrics = SurfpoolMetrics::new(meter);

            if let Err(e) = METER_PROVIDER.set(provider) {
                result = Err(format!("Meter provider already initialized: {:?}", e).into());
                return;
            }
            if let Err(e) = METRICS.set(metrics) {
                result = Err(format!("Metrics already initialized: {:?}", e).into());
                return;
            }

            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("Failed to create tokio runtime: {}", e);
                        return;
                    }
                };
                rt.block_on(async {
                    let registry_clone = registry.clone();
                    let app = axum::Router::new().route(
                        "/metrics",
                        axum::routing::get(move || {
                            let reg = registry_clone.clone();
                            async move {
                                let encoder = prometheus::TextEncoder::new();
                                let metric_families = reg.gather();
                                let mut buffer = vec![];
                                if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
                                    return (
                                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                        format!("Failed to encode: {}", e),
                                    );
                                }
                                let body = String::from_utf8(buffer)
                                    .unwrap_or_else(|_| "Invalid UTF8".to_string());
                                (axum::http::StatusCode::OK, body)
                            }
                        }),
                    );
                    let listener = match tokio::net::TcpListener::bind(&bind_addr_owned).await {
                        Ok(l) => l,
                        Err(e) => {
                            eprintln!("Failed to bind: {}", e);
                            return;
                        }
                    };
                    if let Err(e) = axum::serve(listener, app).await {
                        eprintln!("Server error: {}", e);
                    }
                });
            });
        });
        result
    }

    pub fn metrics() -> Option<&'static SurfpoolMetrics> {
        METRICS.get()
    }

    pub fn shutdown() {
        if let Some(provider) = METER_PROVIDER.get() {
            let _ = provider.shutdown();
        }
    }
}

pub use instrumented::*;

pub fn init_from_config(enabled: bool, bind_addr: &str) -> Result<(), String> {
    if !enabled {
        log::info!("Prometheus metrics disabled");
        return Ok(());
    }
    log::info!("Starting Prometheus metrics on {}", bind_addr);
    init_prometheus("surfpool", bind_addr).map_err(|e| format!("Prometheus init failed: {}", e))
}
