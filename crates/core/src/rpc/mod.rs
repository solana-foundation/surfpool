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
use surfpool_types::{CheatcodeConfig, SimnetCommand, SimnetEvent, types::RpcConfig};

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

        Either::Right(next(request, meta))
    }

    fn on_call<F, X>(
        &self,
        call: Call,
        meta: Option<RunloopContext>,
        next: F,
    ) -> Either<Self::CallFuture, X>
    where
        F: Fn(Call, Option<RunloopContext>) -> X + Send + Sync,
        X: Future<Output = Option<Output>> + Send + 'static,
    {
        let (method_name, id, version) = match &call {
            Call::MethodCall(call) => (call.method.clone(), Some(call.id.clone()), call.jsonrpc),
            Call::Notification(call) => (call.method.clone(), None, call.jsonrpc),
            Call::Invalid { .. } => return Either::Right(next(call, meta)),
        };

        debug!("Processing request '{}'", method_name);

        let error = if method_name.starts_with("surfnet_") {
            meta.as_ref()
                .and_then(|meta| match meta.cheatcode_config.lock() {
                    Ok(config) if config.is_cheatcode_disabled(&method_name) => Some(Error {
                        code: ErrorCode::InvalidRequest,
                        message: format!(
                            "Cheatcode rpc method: {method_name} is currently disabled"
                        ),
                        data: None,
                    }),
                    Ok(_) => None,
                    Err(_) => Some(Error {
                        code: ErrorCode::InternalError,
                        message: "An internal server error occured".to_string(),
                        data: None,
                    }),
                })
        } else {
            None
        };

        if let Some(error) = error {
            warn!("Request rejected due to cheatcode rpc method being disabled");
            let output = id.map(|id| Output::from(Err(error), id, version));
            return Either::Left(Box::pin(async move { output }));
        }

        Either::Left(Box::pin(next(call, meta).map(move |output| {
            if let Some(Output::Failure(failure)) = &output {
                debug!(
                    "RPC error for method '{}': code={:?}, message={}",
                    method_name, failure.error.code, failure.error.message
                );
            }
            output
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
        let _ = self
            .runloop_context
            .svm_locker
            .simnet_events_tx()
            .send(SimnetEvent::debug(msg));
    }

    pub fn log_warn(&self, msg: &str) {
        let _ = self
            .runloop_context
            .svm_locker
            .simnet_events_tx()
            .send(SimnetEvent::warn(msg));
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
mod middleware_tests {
    use crossbeam_channel::unbounded;
    use jsonrpc_core::MetaIoHandler;
    use serde_json::Value;
    use surfpool_types::RpcConfig;

    use super::SurfpoolMiddleware;
    use crate::surfnet::{PluginCommand, locker::SurfnetSvmLocker, svm::SurfnetSvm};

    #[test]
    fn http_middleware_preserves_json_rpc_batch_responses() {
        let (surfnet_svm, _, _) = SurfnetSvm::default();
        let (simnet_commands_tx, _) = unbounded();
        let (plugin_commands_tx, _) = unbounded::<PluginCommand>();
        let middleware = SurfpoolMiddleware::new(
            SurfnetSvmLocker::new(surfnet_svm),
            &simnet_commands_tx,
            &RpcConfig::default(),
            &None,
            plugin_commands_tx,
        );
        let mut io = MetaIoHandler::with_middleware(middleware);
        io.add_sync_method("getSlot", |_| Ok(Value::from(123)));

        let response = io
            .handle_request_sync(
                r#"[{"jsonrpc":"2.0","id":1,"method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"getSlot"}]"#,
                None,
            )
            .expect("a JSON-RPC batch must return one response per method call");

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            serde_json::json!([
                {"jsonrpc": "2.0", "result": 123, "id": 1},
                {"jsonrpc": "2.0", "result": 123, "id": 2}
            ])
        );
    }

    #[test]
    fn http_middleware_omits_batch_notification_responses() {
        let (surfnet_svm, _, _) = SurfnetSvm::default();
        let (simnet_commands_tx, _) = unbounded();
        let (plugin_commands_tx, _) = unbounded::<PluginCommand>();
        let middleware = SurfpoolMiddleware::new(
            SurfnetSvmLocker::new(surfnet_svm),
            &simnet_commands_tx,
            &RpcConfig::default(),
            &None,
            plugin_commands_tx,
        );
        let mut io = MetaIoHandler::with_middleware(middleware);
        io.add_sync_method("getSlot", |_| Ok(Value::from(123)));

        let response = io
            .handle_request_sync(
                r#"[{"jsonrpc":"2.0","method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"getSlot"}]"#,
                None,
            )
            .expect("the method call in a mixed batch must return a response");

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            serde_json::json!([{"jsonrpc": "2.0", "result": 123, "id": 2}])
        );
    }

    #[test]
    fn http_middleware_isolates_errors_within_json_rpc_batches() {
        let (surfnet_svm, _, _) = SurfnetSvm::default();
        let (simnet_commands_tx, _) = unbounded();
        let (plugin_commands_tx, _) = unbounded::<PluginCommand>();
        let middleware = SurfpoolMiddleware::new(
            SurfnetSvmLocker::new(surfnet_svm),
            &simnet_commands_tx,
            &RpcConfig::default(),
            &None,
            plugin_commands_tx,
        );
        let mut io = MetaIoHandler::with_middleware(middleware);
        io.add_sync_method("getSlot", |_| Ok(Value::from(123)));

        let response = io
            .handle_request_sync(
                r#"[{"jsonrpc":"2.0","id":1,"method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"unknownMethod"}]"#,
                None,
            )
            .expect("one unknown method must not discard successful batch responses");

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            serde_json::json!([
                {"jsonrpc": "2.0", "result": 123, "id": 1},
                {
                    "jsonrpc": "2.0",
                    "error": {"code": -32601, "message": "Method not found"},
                    "id": 2
                }
            ])
        );
    }

    #[test]
    fn http_middleware_enforces_cheatcode_policy_per_batch_call() {
        let (surfnet_svm, _, _) = SurfnetSvm::default();
        let (simnet_commands_tx, _) = unbounded();
        let (plugin_commands_tx, _) = unbounded::<PluginCommand>();
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
            .disable_cheatcode(&"surfnet_test".to_string())
            .unwrap();
        let mut io = MetaIoHandler::with_middleware(middleware);
        io.add_sync_method("getSlot", |_| Ok(Value::from(123)));
        io.add_sync_method("surfnet_test", |_| Ok(Value::Bool(true)));

        let response = io
            .handle_request_sync(
                r#"[{"jsonrpc":"2.0","id":1,"method":"getSlot"},{"jsonrpc":"2.0","id":2,"method":"surfnet_test"}]"#,
                None,
            )
            .expect("a disabled cheatcode must not discard other batch responses");

        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            serde_json::json!([
                {"jsonrpc": "2.0", "result": 123, "id": 1},
                {
                    "jsonrpc": "2.0",
                    "error": {
                        "code": -32600,
                        "message": "Cheatcode rpc method: surfnet_test is currently disabled"
                    },
                    "id": 2
                }
            ])
        );
    }
}
