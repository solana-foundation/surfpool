#![allow(unused_imports, unused_variables)]
use std::{
    collections::HashMap,
    error::Error as StdError,
    sync::{Arc, RwLock},
    thread::JoinHandle,
    time::Duration,
};

use actix_cors::Cors;
use actix_web::{
    App, Error, HttpRequest, HttpResponse, HttpServer, Responder,
    dev::ServerHandle,
    http::header::{self},
    middleware, post,
    web::{self, Data, route},
};
use convert_case::{Case, Casing};
use crossbeam::channel::{Receiver, Select, Sender};
use juniper_actix::{graphiql_handler, graphql_handler, subscriptions};
use juniper_graphql_ws::ConnectionConfig;
use log::{debug, error, info, trace, warn};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp_actix_web::transport::StreamableHttpService;
#[cfg(feature = "explorer")]
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use surfpool_core::scenarios::TemplateRegistry;
use surfpool_mcp::Surfpool;
use surfpool_studio_ui::serve_studio_static_files;
use surfpool_types::{
    DataIndexingCommand, OverrideTemplate, SanitizedConfig, Scenario, SubgraphEvent, SurfpoolConfig,
};
use txtx_core::kit::types::types::Value;
use txtx_gql::kit::uuid::Uuid;

use crate::cli::Context;

#[cfg(feature = "explorer")]
#[derive(RustEmbed)]
#[folder = "../../../explorer/.next/server/app"]
pub struct Asset;

/// Registers the studio API routes. Shared between the server and its tests
fn configure_api(cfg: &mut web::ServiceConfig) {
    cfg.service(get_config)
        .service(get_scenario_templates)
        .service(post_scenarios)
        .service(get_scenarios)
        .service(delete_scenario)
        .service(patch_scenario)
        // Unknown /v1/* paths must fail loudly here: otherwise the studio
        // SPA fallback answers them with index.html and a misleading 200
        .service(web::scope("/v1").default_service(web::route().to(api_not_found)));
}

pub async fn start_studio_and_scenario_server(
    network_binding: String,
    config: SanitizedConfig,
    subgraph_events_tx: Sender<SubgraphEvent>,
    ctx: &Context,
    enable_studio: bool,
) -> Result<ServerHandle, Box<dyn StdError>> {
    let config_wrapped = Data::new(RwLock::new(config.clone()));

    // Initialize template registry and load templates
    let template_registry_wrapped = Data::new(RwLock::new(TemplateRegistry::new()));
    let loaded_scenarios = Data::new(RwLock::new(LoadedScenarios::new()));

    // Initialize MCP service
    let mcp_service = StreamableHttpService::builder()
        .service_factory(Arc::new(|| Ok(Surfpool::new())))
        .session_manager(Arc::new(LocalSessionManager::default()))
        .stateful_mode(true)
        .sse_keep_alive(Duration::from_secs(30))
        .build();

    let default_port = network_binding
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or("18488");

    let final_bind_addr = match std::env::var("SURFPOOL_STUDIO_HOST") {
        Ok(host) => {
            let resolved = if host.contains(']') {
                if host.rfind(':') > host.rfind(']') {
                    host
                } else {
                    format!("{}:{}", host, default_port)
                }
            } else {
                match host.matches(':').count() {
                    0 => format!("{}:{}", host, default_port),
                    1 => {
                        if host.rsplit_once(':').map_or(false, |(_, p)| !p.is_empty()) {
                            host
                        } else {
                            format!("{}:{}", host.trim_end_matches(':'), default_port)
                        }
                    }
                    _ => format!("[{}]:{}", host, default_port),
                }
            };
            info!(
                "Binding studio server to {} (from SURFPOOL_STUDIO_HOST)",
                resolved
            );
            resolved
        }
        Err(_) => network_binding,
    };
    info!("Binding studio server to {}", final_bind_addr);
    let server = HttpServer::new(move || {
        let mut app = App::new()
            .app_data(config_wrapped.clone())
            .app_data(template_registry_wrapped.clone())
            .app_data(loaded_scenarios.clone())
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_method()
                    .allow_any_header()
                    .expose_headers(vec!["Mcp-Session-Id", "mcp-session-id"])
                    .supports_credentials()
                    .max_age(3600),
            )
            .wrap(middleware::Compress::default())
            .wrap(middleware::Logger::default())
            .configure(configure_api)
            .service(web::scope("/mcp").service(mcp_service.clone().scope()));

        if enable_studio {
            app = app.app_data(Arc::new(RwLock::new(LoadedScenarios::new())));
            app = app.service(serve_studio_static_files);
        }

        app
    })
    .workers(5)
    .bind(final_bind_addr)?
    .run();
    let handle = server.handle();
    tokio::spawn(server);
    Ok(handle)
}

#[cfg(feature = "explorer")]
fn handle_embedded_file(path: &str) -> HttpResponse {
    use mime_guess::from_path;
    match Asset::get(path) {
        Some(content) => HttpResponse::Ok()
            .content_type(from_path(path).first_or_octet_stream().as_ref())
            .body(content.data.into_owned()),
        None => {
            if let Some(index_content) = Asset::get("index.html") {
                HttpResponse::Ok()
                    .content_type("text/html")
                    .body(index_content.data.into_owned())
            } else {
                HttpResponse::NotFound().body("404 Not Found")
            }
        }
    }
}

#[actix_web::get("/config")]
async fn get_config(
    req: HttpRequest,
    payload: web::Payload,
    config: Data<RwLock<SanitizedConfig>>,
) -> Result<HttpResponse, Error> {
    let config = config
        .read()
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to read context"))?;
    let api_config = serde_json::json!(*config);
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(api_config.to_string()))
}

#[actix_web::get("/v1/scenarios/templates")]
async fn get_scenario_templates(
    template_registry: Data<RwLock<TemplateRegistry>>,
) -> Result<HttpResponse, Error> {
    let registry = template_registry.read().map_err(|_| {
        actix_web::error::ErrorInternalServerError("Failed to read template registry")
    })?;

    let templates: Vec<&OverrideTemplate> = registry.all();
    let response = serde_json::to_string(&templates)
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to serialize templates"))?;

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(response))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoadedScenarios {
    pub scenarios: Vec<Scenario>,
}
impl LoadedScenarios {
    pub fn new() -> Self {
        Self {
            scenarios: Vec::new(),
        }
    }
}

#[post("/v1/scenarios")]
async fn post_scenarios(
    req: HttpRequest,
    scenario: web::Json<Scenario>,
    data: Data<RwLock<LoadedScenarios>>,
) -> Result<HttpResponse, Error> {
    let mut loaded_scenarios = data
        .write()
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to acquire write lock"))?;
    let scenario_data = scenario.into_inner();
    let scenario_id = scenario_data.id.clone();

    if let Some(existing) = loaded_scenarios
        .scenarios
        .iter()
        .find(|s| s.id == scenario_id)
    {
        let identical = match (
            serde_json::to_value(existing),
            serde_json::to_value(&scenario_data),
        ) {
            (Ok(stored), Ok(incoming)) => stored == incoming,
            _ => false,
        };

        if identical {
            let response = serde_json::json!({"id": scenario_id});
            return Ok(HttpResponse::Ok()
                .content_type("application/json")
                .body(response.to_string()));
        }

        let response = serde_json::json!({
            "error": "a different scenario is already stored under this id",
            "id": scenario_id,
        });
        return Ok(HttpResponse::Conflict()
            .content_type("application/json")
            .body(response.to_string()));
    }

    loaded_scenarios.scenarios.push(scenario_data);
    let response = serde_json::json!({"id": scenario_id});
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(response.to_string()))
}

#[actix_web::get("/v1/scenarios")]
async fn get_scenarios(data: Data<RwLock<LoadedScenarios>>) -> Result<HttpResponse, Error> {
    let loaded_scenarios = data
        .read()
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to acquire read lock"))?;
    let response = serde_json::to_string(&loaded_scenarios.scenarios).map_err(|_| {
        actix_web::error::ErrorInternalServerError("Failed to serialize loaded scenarios")
    })?;

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(response))
}

#[actix_web::delete("/v1/scenarios/{id}")]
async fn delete_scenario(
    path: web::Path<String>,
    data: Data<RwLock<LoadedScenarios>>,
) -> Result<HttpResponse, Error> {
    let scenario_id = path.into_inner();
    let mut loaded_scenarios = data
        .write()
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to acquire write lock"))?;

    let initial_len = loaded_scenarios.scenarios.len();
    loaded_scenarios.scenarios.retain(|s| s.id != scenario_id);

    if loaded_scenarios.scenarios.len() == initial_len {
        return Ok(
            HttpResponse::NotFound().body(format!("Scenario with id '{}' not found", scenario_id))
        );
    }

    Ok(HttpResponse::Ok().body(format!("Scenario '{}' deleted", scenario_id)))
}

fn merge_scenario_patch(
    existing: &Scenario,
    patch: &serde_json::Value,
    path_id: &str,
) -> Result<Scenario, String> {
    let patch = patch
        .as_object()
        .ok_or_else(|| "PATCH body must be a JSON object".to_string())?;
    let mut merged = serde_json::to_value(existing).map_err(|e| e.to_string())?;
    let obj = merged
        .as_object_mut()
        .ok_or_else(|| "Failed to serialize the stored scenario".to_string())?;
    for (key, value) in patch {
        if key != "id" && !obj.contains_key(key) {
            return Err(format!("Unknown scenario field '{key}'"));
        }
        obj.insert(key.clone(), value.clone());
    }
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(path_id.to_string()),
    );
    deserialize_scenario_strictly(merged)
}

fn scenario_from_full_patch(
    mut patch: serde_json::Value,
    path_id: &str,
) -> Result<Scenario, String> {
    let obj = patch
        .as_object_mut()
        .ok_or_else(|| "PATCH body must be a JSON object".to_string())?;
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(path_id.to_string()),
    );
    deserialize_scenario_strictly(patch)
}

fn deserialize_scenario_strictly(value: serde_json::Value) -> Result<Scenario, String> {
    let scenario: Scenario = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let normalized = serde_json::to_value(&scenario).map_err(|e| e.to_string())?;
    validate_json_fields(&value, &normalized, "scenario")?;
    Ok(scenario)
}

fn validate_json_fields(
    supplied: &serde_json::Value,
    normalized: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    match (supplied, normalized) {
        (serde_json::Value::Object(supplied), serde_json::Value::Object(normalized)) => {
            for (key, value) in supplied {
                let normalized_value = normalized
                    .get(key)
                    .ok_or_else(|| format!("Unknown field '{path}.{key}'"))?;
                validate_json_fields(value, normalized_value, &format!("{path}.{key}"))?;
            }
        }
        (serde_json::Value::Array(supplied), serde_json::Value::Array(normalized)) => {
            for (index, (value, normalized_value)) in
                supplied.iter().zip(normalized.iter()).enumerate()
            {
                validate_json_fields(value, normalized_value, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_error(status: actix_web::http::StatusCode, message: String) -> HttpResponse {
    HttpResponse::build(status)
        .content_type("application/json")
        .body(serde_json::json!({ "error": message }).to_string())
}

#[actix_web::patch("/v1/scenarios/{id}")]
async fn patch_scenario(
    path: web::Path<String>,
    patch: web::Json<serde_json::Value>,
    data: Data<RwLock<LoadedScenarios>>,
) -> Result<HttpResponse, Error> {
    let scenario_id = path.into_inner();
    let mut loaded_scenarios = data
        .write()
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to acquire write lock"))?;

    let scenario_index = loaded_scenarios
        .scenarios
        .iter()
        .position(|s| s.id == scenario_id);
    let updated = match scenario_index {
        Some(index) => {
            merge_scenario_patch(&loaded_scenarios.scenarios[index], &patch, &scenario_id)
        }
        None => scenario_from_full_patch(patch.into_inner(), &scenario_id),
    };

    match updated {
        Ok(updated) => {
            if let Some(index) = scenario_index {
                loaded_scenarios.scenarios[index] = updated;
            } else {
                loaded_scenarios.scenarios.push(updated);
            }
            Ok(HttpResponse::Ok()
                .content_type("application/json")
                .body(serde_json::json!({ "id": scenario_id }).to_string()))
        }
        Err(message) => Ok(json_error(
            actix_web::http::StatusCode::BAD_REQUEST,
            message,
        )),
    }
}

#[allow(dead_code)]
#[cfg(not(feature = "explorer"))]
fn handle_embedded_file(_path: &str) -> HttpResponse {
    HttpResponse::NotFound().body("404 Not Found")
}

#[actix_web::get("/{_:.*}")]
async fn dist(path: web::Path<String>) -> impl Responder {
    let path_str = match path.as_str() {
        "" => "index.html",
        other => other,
    };
    handle_embedded_file(path_str)
}

async fn api_not_found() -> HttpResponse {
    HttpResponse::NotFound()
        .content_type("application/json")
        .body(r#"{"error":"not found"}"#)
}

#[cfg(test)]
mod tests {
    use actix_web::{App, test};

    use super::*;

    fn scenario_body() -> serde_json::Value {
        serde_json::json!({
            "id": "s1",
            "name": "first",
            "description": "",
            "overrides": [],
            "tags": [],
        })
    }

    fn post_scenario(body: serde_json::Value) -> test::TestRequest {
        test::TestRequest::post()
            .uri("/v1/scenarios")
            .set_json(body)
    }

    #[actix_web::test]
    async fn creating_the_same_scenario_twice_is_a_no_op() {
        let loaded_scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(loaded_scenarios.clone())
                .configure(configure_api),
        )
        .await;

        let created = test::call_service(&app, post_scenario(scenario_body()).to_request()).await;
        assert_eq!(created.status(), 200, "first create must succeed");

        let retried = test::call_service(&app, post_scenario(scenario_body()).to_request()).await;
        assert_eq!(
            retried.status(),
            200,
            "an identical retry must be accepted as a no-op"
        );

        let stored = &loaded_scenarios.read().unwrap().scenarios;
        assert_eq!(stored.len(), 1, "no duplicate may be stored");
    }

    #[actix_web::test]
    async fn reusing_a_scenario_id_for_different_content_conflicts() {
        let loaded_scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(loaded_scenarios.clone())
                .configure(configure_api),
        )
        .await;

        let created = test::call_service(&app, post_scenario(scenario_body()).to_request()).await;
        assert_eq!(created.status(), 200, "first create must succeed");

        let mut conflicting = scenario_body();
        conflicting["name"] = serde_json::json!("second");
        let rejected = test::call_service(&app, post_scenario(conflicting).to_request()).await;
        assert_eq!(
            rejected.status(),
            409,
            "different content under a taken id must conflict"
        );

        let stored = &loaded_scenarios.read().unwrap().scenarios;
        assert_eq!(
            stored.len(),
            1,
            "the conflicting scenario must not be stored"
        );
        assert_eq!(stored[0].name, "first", "the stored scenario is untouched");
    }

    #[actix_web::test]
    async fn unknown_v1_paths_return_json_404_instead_of_spa_fallback() {
        let loaded_scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(loaded_scenarios)
                .configure(configure_api)
                .service(surfpool_studio_ui::serve_studio_static_files),
        )
        .await;

        for path in ["/v1/scenarios/some-id", "/v1/nonexistent"] {
            let request = test::TestRequest::get().uri(path).to_request();
            let response = test::call_service(&app, request).await;
            assert_eq!(response.status(), 404, "expected 404 for {path}");
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                "application/json",
                "expected JSON body for {path}"
            );
        }

        let request = test::TestRequest::post()
            .uri("/v1/nonexistent")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(
            response.status(),
            404,
            "the guard must catch non-GET methods too"
        );
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json",
        );

        let request = test::TestRequest::get().uri("/v1/scenarios").to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(
            response.status(),
            200,
            "registered endpoints must keep working"
        );
    }

    fn scenario_json(id: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "description": "original description",
            "overrides": [{
                "id": "o1",
                "templateId": "pyth-price-feed-v2",
                "values": {},
                "scenarioRelativeSlot": 0,
                "enabled": true,
                "fetchBeforeUse": false,
                "account": { "pubkey": "So11111111111111111111111111111111111111112" }
            }],
            "tags": ["repro"],
        })
    }

    fn scenario_of(json: serde_json::Value) -> Scenario {
        serde_json::from_value(json).expect("valid scenario")
    }

    #[actix_web::test]
    async fn partial_patch_preserves_omitted_fields() {
        let existing = scenario_of(scenario_json("s1", "before"));
        let patch = serde_json::json!({ "name": "after" });

        let merged = merge_scenario_patch(&existing, &patch, "s1").unwrap();

        assert_eq!(merged.name, "after", "the sent field changes");
        assert_eq!(merged.overrides.len(), 1, "omitted overrides survive");
        assert_eq!(merged.tags, vec!["repro"], "omitted tags survive");
        assert_eq!(merged.description, "original description");
    }

    #[actix_web::test]
    async fn full_patch_replaces_all_field_values() {
        let existing = scenario_of(scenario_json("s1", "original"));
        let patch = serde_json::json!({
            "id": "s1", "name": "after", "description": "", "overrides": [], "tags": []
        });

        let merged = merge_scenario_patch(&existing, &patch, "s1").unwrap();

        assert_eq!(merged.name, "after");
        assert!(merged.description.is_empty());
        assert!(merged.overrides.is_empty(), "a full document replaces");
        assert!(merged.tags.is_empty());
    }

    #[actix_web::test]
    async fn explicit_empty_array_patch_clears_scenario_overrides() {
        let existing = scenario_of(scenario_json("s1", "original"));
        let patch = serde_json::json!({ "overrides": [] });

        let merged = merge_scenario_patch(&existing, &patch, "s1").unwrap();

        assert!(merged.overrides.is_empty(), "an explicit [] clears");
        assert_eq!(merged.name, "original", "other fields untouched");
    }

    #[actix_web::test]
    async fn path_id_wins_over_body_id() {
        let existing = scenario_of(scenario_json("s1", "before"));
        let patch = serde_json::json!({ "id": "s2", "name": "after" });

        let merged = merge_scenario_patch(&existing, &patch, "s1").unwrap();

        assert_eq!(
            merged.id, "s1",
            "the path id is authoritative, not the body id"
        );
    }

    #[actix_web::test]
    async fn non_object_body_is_rejected() {
        let existing = scenario_of(scenario_json("s1", "before"));
        assert!(merge_scenario_patch(&existing, &serde_json::json!([1, 2, 3]), "s1").is_err());
        assert!(merge_scenario_patch(&existing, &serde_json::json!("nope"), "s1").is_err());
    }

    #[actix_web::test]
    async fn a_wrong_typed_field_is_rejected() {
        let existing = scenario_of(scenario_json("s1", "before"));
        let patch = serde_json::json!({ "name": 123 });
        assert!(
            merge_scenario_patch(&existing, &patch, "s1").is_err(),
            "a merge that yields an invalid Scenario must fail, not store garbage"
        );
    }

    #[actix_web::test]
    async fn an_unknown_field_is_rejected() {
        let existing = scenario_of(scenario_json("s1", "before"));
        let patch = serde_json::json!({ "unknown": true });

        assert!(merge_scenario_patch(&existing, &patch, "s1").is_err());
    }

    #[actix_web::test]
    async fn a_nested_unknown_override_field_is_rejected_without_mutating() {
        let scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(scenarios.clone())
                .configure(configure_api),
        )
        .await;
        let original = scenario_json("s1", "before");
        let create = test::TestRequest::post()
            .uri("/v1/scenarios")
            .set_json(&original)
            .to_request();
        assert_eq!(test::call_service(&app, create).await.status(), 200);

        let mut invalid_override = original["overrides"][0].clone();
        invalid_override["fetchBeforeUes"] = serde_json::json!(true);
        let patch = test::TestRequest::patch()
            .uri("/v1/scenarios/s1")
            .set_json(serde_json::json!({ "overrides": [invalid_override] }))
            .to_request();
        let response = test::call_service(&app, patch).await;

        assert_eq!(response.status(), 400);
        assert_eq!(
            scenarios.read().unwrap().scenarios,
            vec![scenario_of(original)]
        );
    }

    #[actix_web::test]
    async fn arbitrary_nested_override_values_remain_valid() {
        let mut patch = scenario_json("s1", "with arbitrary values");
        patch["overrides"][0]["values"] = serde_json::json!({
            "custom": {
                "futureField": true,
                "nested": [1, 2, 3]
            }
        });

        let scenario = scenario_from_full_patch(patch.clone(), "s1").unwrap();

        assert_eq!(
            scenario.overrides[0].values["custom"],
            patch["overrides"][0]["values"]["custom"]
        );
    }

    #[actix_web::test]
    async fn a_full_document_patch_preserves_upsert_compatibility() {
        let scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(scenarios.clone())
                .configure(configure_api),
        )
        .await;

        let request = test::TestRequest::patch()
            .uri("/v1/scenarios/ghost")
            .set_json(scenario_json("body-id", "created by patch"))
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(response.status(), 200);
        let stored = &scenarios.read().unwrap().scenarios;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, "ghost", "the path id remains authoritative");
    }

    #[actix_web::test]
    async fn a_full_document_upsert_rejects_unknown_fields_without_mutating() {
        let scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(scenarios.clone())
                .configure(configure_api),
        )
        .await;
        let mut patch = scenario_json("body-id", "invalid upsert");
        patch["unknown"] = serde_json::json!(true);

        let request = test::TestRequest::patch()
            .uri("/v1/scenarios/ghost")
            .set_json(patch)
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(response.status(), 400);
        assert!(scenarios.read().unwrap().scenarios.is_empty());
    }

    #[actix_web::test]
    async fn a_partial_patch_cannot_create_an_incomplete_scenario() {
        let scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(scenarios.clone())
                .configure(configure_api),
        )
        .await;

        let request = test::TestRequest::patch()
            .uri("/v1/scenarios/ghost")
            .set_json(serde_json::json!({ "name": "incomplete" }))
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(response.status(), 400);
        assert!(scenarios.read().unwrap().scenarios.is_empty());
    }

    #[actix_web::test]
    async fn consecutive_partial_patches_preserve_omitted_fields() {
        let scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(scenarios.clone())
                .configure(configure_api),
        )
        .await;

        let create = test::TestRequest::post()
            .uri("/v1/scenarios")
            .set_json(scenario_json("s1", "before"))
            .to_request();
        assert_eq!(test::call_service(&app, create).await.status(), 200);

        let rename = test::TestRequest::patch()
            .uri("/v1/scenarios/s1")
            .set_json(serde_json::json!({ "name": "renamed" }))
            .to_request();
        assert_eq!(test::call_service(&app, rename).await.status(), 200);

        let describe = test::TestRequest::patch()
            .uri("/v1/scenarios/s1")
            .set_json(serde_json::json!({ "description": "updated description" }))
            .to_request();
        assert_eq!(test::call_service(&app, describe).await.status(), 200);

        let stored = &scenarios.read().unwrap().scenarios;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "renamed");
        assert_eq!(stored[0].description, "updated description");
        assert_eq!(
            stored[0].overrides.len(),
            1,
            "a rename must not wipe overrides"
        );
        assert_eq!(stored[0].tags, vec!["repro"], "a rename must not wipe tags");
    }

    #[actix_web::test]
    async fn patch_with_a_different_body_id_does_not_rename_the_record() {
        let scenarios = Data::new(RwLock::new(LoadedScenarios::new()));
        let app = test::init_service(
            App::new()
                .app_data(scenarios.clone())
                .configure(configure_api),
        )
        .await;

        let create = test::TestRequest::post()
            .uri("/v1/scenarios")
            .set_json(scenario_json("s1", "before"))
            .to_request();
        assert_eq!(test::call_service(&app, create).await.status(), 200);

        let patch = test::TestRequest::patch()
            .uri("/v1/scenarios/s1")
            .set_json(serde_json::json!({ "id": "s2", "name": "after" }))
            .to_request();
        assert_eq!(test::call_service(&app, patch).await.status(), 200);

        let stored = &scenarios.read().unwrap().scenarios;
        assert_eq!(stored.len(), 1, "no second record may appear");
        assert_eq!(stored[0].id, "s1", "the id in the path is authoritative");
    }
}
