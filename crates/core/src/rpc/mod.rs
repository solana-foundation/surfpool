use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use blake3::Hash;
use crossbeam_channel::Sender;
use jsonrpc_core::{
    BoxFuture, Call, Error, ErrorCode, FutureResponse, Metadata, Middleware, Output, Request,
    Response,
    futures::{FutureExt, future::Either},
    middleware,
};
use jsonrpc_pubsub::{PubSubMetadata, Session};
use solana_clock::Slot;
use surfpool_types::{CheatcodeConfig, SimnetCommand, types::RpcConfig};

use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::{
        PluginCommand,
        locker::SurfnetSvmLocker,
        remote::{SomeRemoteCtx, SurfnetRemoteClient},
        svm::SurfnetSvm,
    },
};

pub mod accounts_data;
pub mod accounts_scan;
pub mod admin;
pub mod bank_data;
pub mod full;
pub mod jito;
pub mod minimal;
pub mod surfnet_cheatcodes;
pub mod utils;
pub mod ws;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum RpcHealthStatus {
    Ok,
    Behind { num_slots: Slot },
    Unknown,
}

pub struct SurfpoolRpc;

#[derive(Clone)]
pub struct RunloopContext {
    pub id: Option<(Hash, String)>,
    pub svm_locker: SurfnetSvmLocker,
    pub simnet_commands_tx: Sender<SimnetCommand>,
    pub remote_rpc_client: Option<SurfnetRemoteClient>,
    pub rpc_config: RpcConfig,
    pub cheatcode_config: Arc<Mutex<CheatcodeConfig>>,
    pub plugin_commands_tx: Sender<PluginCommand>,
}

pub struct SurfnetRpcContext<T> {
    pub svm_locker: SurfnetSvmLocker,
    pub remote_ctx: Option<(SurfnetRemoteClient, T)>,
}

trait State {
    fn get_svm_locker(&self) -> SurfpoolResult<SurfnetSvmLocker>;
    fn with_svm_reader<T, F>(&self, reader: F) -> Result<T, SurfpoolError>
    where
        F: Fn(&SurfnetSvm) -> T + Send + Sync,
        T: Send + 'static;
    fn get_rpc_context<T>(&self, input: T) -> SurfpoolResult<SurfnetRpcContext<T>>;
    fn get_surfnet_command_tx(&self) -> SurfpoolResult<Sender<SimnetCommand>>;
}

impl State for Option<RunloopContext> {
    fn get_svm_locker(&self) -> SurfpoolResult<SurfnetSvmLocker> {
        // Retrieve svm state
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };
        Ok(ctx.svm_locker.clone())
    }

    fn with_svm_reader<T, F>(&self, reader: F) -> Result<T, SurfpoolError>
    where
        F: Fn(&SurfnetSvm) -> T + Send + Sync,
        T: Send + 'static,
    {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };
        Ok(ctx.svm_locker.with_svm_reader(reader))
    }

    fn get_rpc_context<T>(&self, input: T) -> SurfpoolResult<SurfnetRpcContext<T>> {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };

        Ok(SurfnetRpcContext {
            svm_locker: ctx.svm_locker.clone(),
            remote_ctx: ctx.remote_rpc_client.get_remote_ctx(input),
        })
    }

    fn get_surfnet_command_tx(&self) -> SurfpoolResult<Sender<SimnetCommand>> {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };
        Ok(ctx.simnet_commands_tx.clone())
    }
}

impl Metadata for RunloopContext {}

#[derive(Clone)]
pub struct SurfpoolMiddleware {
    pub surfnet_svm: SurfnetSvmLocker,
    pub simnet_commands_tx: Sender<SimnetCommand>,
    pub config: RpcConfig,
    pub remote_rpc_client: Option<SurfnetRemoteClient>,
    pub cheatcode_config: Arc<Mutex<CheatcodeConfig>>,
    pub plugin_commands_tx: Sender<PluginCommand>,
}

impl SurfpoolMiddleware {
    pub fn new(
        surfnet_svm: SurfnetSvmLocker,
        simnet_commands_tx: &Sender<SimnetCommand>,
        config: &RpcConfig,
        remote_rpc_client: &Option<SurfnetRemoteClient>,
        plugin_commands_tx: Sender<PluginCommand>,
    ) -> Self {
        Self {
            surfnet_svm,
            simnet_commands_tx: simnet_commands_tx.clone(),
            config: config.clone(),
            remote_rpc_client: remote_rpc_client.clone(),
            cheatcode_config: CheatcodeConfig::new(),
            plugin_commands_tx,
        }
    }

    /// `Some` when the call must not reach the handler. Cheatcode gating sits above the method
    /// table rather than in it, so it has to run once per batch element, not once per request.
    fn disabled_cheatcode_error(&self, method_name: &str) -> Option<Error> {
        if !method_name.starts_with("surfnet_") {
            return None;
        }

        let Ok(cheatcode_config) = self.cheatcode_config.lock() else {
            warn!("Request rejected due to cheatcode being disabled");
            return Some(Error {
                code: ErrorCode::InternalError,
                message: "An internal server error occured".to_string(),
                data: None,
            });
        };

        if !cheatcode_config.is_cheatcode_disabled(&method_name.to_string()) {
            return None;
        }

        warn!("Request rejected due to cheatcode rpc method being disabled");
        Some(Error {
            code: ErrorCode::InvalidRequest,
            message: format!("Cheatcode rpc method: {method_name} is currently disabled"),
            data: None,
        })
    }

    fn dispatch_batch<F, X>(
        &self,
        calls: Vec<Call>,
        meta: Option<RunloopContext>,
        next: F,
    ) -> Either<FutureResponse, X>
    where
        F: FnOnce(Request, Option<RunloopContext>) -> X + Send,
        X: Future<Output = Option<Response>> + Send + 'static,
    {
        let mut forwarded = Vec::with_capacity(calls.len());
        let mut rejected = Vec::new();

        for call in calls {
            // A malformed element carries no method to gate; the handler answers it per element.
            let method_name = match &call {
                Call::MethodCall(method_call) => Some(method_call.method.as_str()),
                Call::Notification(notification) => Some(notification.method.as_str()),
                Call::Invalid { .. } => None,
            };

            match method_name.and_then(|name| self.disabled_cheatcode_error(name)) {
                None => forwarded.push(call),
                // A notification is answered by nothing at all, gated or not.
                Some(error) => {
                    if let Call::MethodCall(method_call) = call {
                        rejected.push(Output::from(
                            Err(error),
                            method_call.id,
                            method_call.jsonrpc,
                        ));
                    }
                }
            }
        }

        if forwarded.is_empty() {
            let response = (!rejected.is_empty()).then_some(Response::Batch(rejected));
            return Either::Left(Box::pin(async move { response }));
        }

        // Order is not part of the batch contract: clients correlate on `id`.
        Either::Left(Box::pin(next(Request::Batch(forwarded), meta).map(
            move |res| match res {
                Some(Response::Batch(mut outputs)) => {
                    outputs.extend(rejected);
                    Some(Response::Batch(outputs))
                }
                _ => (!rejected.is_empty()).then_some(Response::Batch(rejected)),
            },
        )))
    }
}

impl Middleware<Option<RunloopContext>> for SurfpoolMiddleware {
    type Future = FutureResponse;
    type CallFuture = middleware::NoopCallFuture;

    fn on_request<F, X>(
        &self,
        request: Request,
        _meta: Option<RunloopContext>,
        next: F,
    ) -> Either<Self::Future, X>
    where
        F: FnOnce(Request, Option<RunloopContext>) -> X + Send,
        X: Future<Output = Option<Response>> + Send + 'static,
    {
        let meta = Some(RunloopContext {
            id: None,
            svm_locker: self.surfnet_svm.clone(),
            simnet_commands_tx: self.simnet_commands_tx.clone(),
            remote_rpc_client: self.remote_rpc_client.clone(),
            rpc_config: self.config.clone(),
            cheatcode_config: self.cheatcode_config.clone(),
            plugin_commands_tx: self.plugin_commands_tx.clone(),
        });

        let Request::Single(Call::MethodCall(ref method_call)) = request else {
            // JSON-RPC 2.0 §6: an empty array is not a batch and answers with one Invalid
            // Request object, which is what the arm below already returns.
            if let Request::Batch(calls) = request
                && !calls.is_empty()
            {
                return self.dispatch_batch(calls, meta, next);
            }

            let error = Response::from(
                Error {
                    code: ErrorCode::InvalidRequest,
                    message: "Only method calls are supported".into(),
                    data: None,
                },
                None,
            );
            warn!("Request rejected due to not being a single method call");

            return Either::Left(Box::pin(async move { Some(error) }));
        };

        let method_name = method_call.method.clone();
        debug!("Processing request '{}'", method_name);

        if let Some(error) = self.disabled_cheatcode_error(&method_name) {
            let error = Response::from(error, None);
            return Either::Left(Box::pin(async move { Some(error) }));
        }

        Either::Left(Box::pin(next(request, meta).map(move |res| {
            if let Some(Response::Single(output)) = &res {
                if let jsonrpc_core::Output::Failure(failure) = output {
                    debug!(
                        "RPC error for method '{}': code={:?}, message={}",
                        method_name, failure.error.code, failure.error.message
                    );
                }
            }
            res
        })))
    }
}

#[derive(Clone)]
pub struct SurfpoolWebsocketMiddleware {
    pub surfpool_middleware: SurfpoolMiddleware,
    pub session: Option<Arc<Session>>,
}

impl SurfpoolWebsocketMiddleware {
    pub fn new(surfpool_middleware: SurfpoolMiddleware, session: Option<Arc<Session>>) -> Self {
        Self {
            surfpool_middleware,
            session,
        }
    }
}

impl Middleware<Option<SurfpoolWebsocketMeta>> for SurfpoolWebsocketMiddleware {
    type Future = FutureResponse;
    type CallFuture = middleware::NoopCallFuture;

    fn on_request<F, X>(
        &self,
        request: Request,
        meta: Option<SurfpoolWebsocketMeta>,
        next: F,
    ) -> Either<Self::Future, X>
    where
        F: FnOnce(Request, Option<SurfpoolWebsocketMeta>) -> X + Send,
        X: Future<Output = Option<Response>> + Send + 'static,
    {
        let runloop_context = RunloopContext {
            id: None,
            svm_locker: self.surfpool_middleware.surfnet_svm.clone(),
            simnet_commands_tx: self.surfpool_middleware.simnet_commands_tx.clone(),
            remote_rpc_client: self.surfpool_middleware.remote_rpc_client.clone(),
            rpc_config: self.surfpool_middleware.config.clone(),
            cheatcode_config: self.surfpool_middleware.cheatcode_config.clone(),
            plugin_commands_tx: self.surfpool_middleware.plugin_commands_tx.clone(),
        };
        let session = meta
            .as_ref()
            .and_then(|m| m.session.clone())
            .or(self.session.clone());
        let meta = Some(SurfpoolWebsocketMeta::new(runloop_context, session));
        Either::Left(Box::pin(next(request, meta).map(move |res| res)))
    }
}

#[derive(Clone)]
pub struct SurfpoolWebsocketMeta {
    pub runloop_context: RunloopContext,
    pub session: Option<Arc<Session>>,
}

impl SurfpoolWebsocketMeta {
    pub fn new(runloop_context: RunloopContext, session: Option<Arc<Session>>) -> Self {
        Self {
            runloop_context,
            session,
        }
    }

    pub fn log_debug(&self, msg: &str) {
        self.runloop_context
            .svm_locker
            .simnet_events_tx()
            .debug(msg);
    }

    pub fn log_warn(&self, msg: &str) {
        self.runloop_context.svm_locker.simnet_events_tx().warn(msg);
    }
}

impl State for Option<SurfpoolWebsocketMeta> {
    fn get_svm_locker(&self) -> SurfpoolResult<SurfnetSvmLocker> {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };
        Ok(ctx.runloop_context.svm_locker.clone())
    }

    fn with_svm_reader<T, F>(&self, reader: F) -> Result<T, SurfpoolError>
    where
        F: Fn(&SurfnetSvm) -> T + Send + Sync,
        T: Send + 'static,
    {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };
        Ok(ctx.runloop_context.svm_locker.with_svm_reader(reader))
    }

    fn get_rpc_context<T>(&self, input: T) -> SurfpoolResult<SurfnetRpcContext<T>> {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };

        Ok(SurfnetRpcContext {
            svm_locker: ctx.runloop_context.svm_locker.clone(),
            remote_ctx: ctx.runloop_context.remote_rpc_client.get_remote_ctx(input),
        })
    }

    fn get_surfnet_command_tx(&self) -> SurfpoolResult<Sender<SimnetCommand>> {
        let Some(ctx) = self else {
            return Err(SurfpoolError::missing_context());
        };
        Ok(ctx.runloop_context.simnet_commands_tx.clone())
    }
}

impl Metadata for SurfpoolWebsocketMeta {}
impl PubSubMetadata for SurfpoolWebsocketMeta {
    fn session(&self) -> Option<Arc<jsonrpc_pubsub::Session>> {
        self.session.clone()
    }
}

pub const NOT_IMPLEMENTED_CODE: i64 = -32051; // -32000 to -32099 are reserved by the json-rpc spec for custom errors
pub const NOT_IMPLEMENTED_MSG: &str = "Method not yet implemented. If this endpoint is a priority for you, please open an issue here so we can prioritize: https://github.com/solana-foundation/surfpool/issues";
fn not_implemented_msg(method: &str) -> String {
    format!(
        "Method `{}` is not yet implemented. If this endpoint is a priority for you, please open an issue here so we can prioritize: https://github.com/solana-foundation/surfpool/issues",
        method
    )
}
/// Helper function to return a `NotImplemented` JSON RPC error
pub fn not_implemented_err<T>(method: &str) -> Result<T, Error> {
    Err(Error {
        code: jsonrpc_core::types::ErrorCode::ServerError(NOT_IMPLEMENTED_CODE),
        message: not_implemented_msg(method),
        data: None,
    })
}

pub fn not_implemented_err_async<T>(method: &str) -> BoxFuture<Result<T, Error>> {
    let method = method.to_string();
    Box::pin(async move {
        Err(Error {
            code: jsonrpc_core::types::ErrorCode::ServerError(NOT_IMPLEMENTED_CODE),
            message: not_implemented_msg(&method),
            data: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use jsonrpc_core::{MetaIoHandler, Value};
    use serde_json::json;

    use super::*;

    /// A handler carrying the real middleware, one plain method and one cheatcode, so the tests
    /// exercise the batch path end to end rather than the middleware in isolation.
    fn test_handler() -> MetaIoHandler<Option<RunloopContext>, SurfpoolMiddleware> {
        let (surfnet_svm, _events_rx, _) = SurfnetSvm::default();
        let (simnet_commands_tx, _rx) = crossbeam_channel::unbounded();
        let (plugin_commands_tx, _rx) = crossbeam_channel::unbounded();

        let middleware = SurfpoolMiddleware::new(
            SurfnetSvmLocker::new(surfnet_svm),
            &simnet_commands_tx,
            &RpcConfig::default(),
            &None,
            plugin_commands_tx,
        );
        middleware
            .cheatcode_config
            .lock()
            .unwrap()
            .disable_cheatcode(&"surfnet_setAccount".to_string())
            .unwrap();

        let mut io = MetaIoHandler::with_middleware(middleware);
        io.add_method_with_meta("getSlot", |_params, _meta| async { Ok(Value::from(45)) });
        io.add_method_with_meta("surfnet_setAccount", |_params, _meta| async {
            Ok(Value::Null)
        });
        io
    }

    #[tokio::test]
    async fn batch_of_method_calls_is_answered_with_an_array() {
        let request = r#"[{"jsonrpc":"2.0","id":1,"method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"getSlot"}]"#;

        let response = test_handler().handle_request(request, None).await.unwrap();

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!([
                {"jsonrpc": "2.0", "result": 45, "id": 1},
                {"jsonrpc": "2.0", "result": 45, "id": 2}
            ])
        );
    }

    #[tokio::test]
    async fn notifications_are_omitted_from_the_batch_response() {
        let request =
            r#"[{"jsonrpc":"2.0","method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"getSlot"}]"#;

        let response = test_handler().handle_request(request, None).await.unwrap();

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!([{"jsonrpc": "2.0", "result": 45, "id": 2}])
        );
    }

    #[tokio::test]
    async fn all_notification_batch_is_answered_with_nothing() {
        let request =
            r#"[{"jsonrpc":"2.0","method":"getSlot"},{"jsonrpc":"2.0","method":"getSlot"}]"#;

        assert_eq!(test_handler().handle_request(request, None).await, None);
    }

    #[tokio::test]
    async fn a_rejected_element_fails_alone() {
        let request = r#"[{"jsonrpc":"2.0","id":1,"method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"surfnet_setAccount"}]"#;

        let response = test_handler().handle_request(request, None).await.unwrap();

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!([
                {"jsonrpc": "2.0", "result": 45, "id": 1},
                {"jsonrpc": "2.0", "error": {
                    "code": -32600,
                    "message": "Cheatcode rpc method: surfnet_setAccount is currently disabled"
                }, "id": 2}
            ])
        );
    }

    #[tokio::test]
    async fn an_empty_batch_is_answered_with_one_invalid_request() {
        let response = test_handler().handle_request("[]", None).await.unwrap();

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!({
                "error": {"code": -32600, "message": "Only method calls are supported"},
                "id": null
            })
        );
    }
}
