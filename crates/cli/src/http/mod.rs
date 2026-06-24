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
#[cfg(feature = "explorer")]
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use surfpool_core::scenarios::TemplateRegistry;
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

    let default_port = network_binding.split(':').last().unwrap_or("18488");
    let final_bind_addr = std::env::var("SURFPOOL_STUDIO_HOST")
        .map(|host| {
            if host.contains(']') {
                if host.rfind(':') > host.rfind(']') {
                    host
                } else {
                    format!("{}:{}", host, default_port)
                }
            } else {
                match host.matches(':').count() {
                    0 => format!("{}:{}", host, default_port),
                    1 => host,
                    _ => format!("[{}]:{}", host, default_port),
                }
            }
        })
        .unwrap_or(network_binding);

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
                    .supports_credentials()
                    .max_age(3600),
            )
            .wrap(middleware::Compress::default())
            .wrap(middleware::Logger::default())
            .service(get_config)
            .service(get_scenario_templates)
            .service(post_scenarios)
            .service(get_scenarios)
            .service(delete_scenario)
            .service(patch_scenario);

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

#[actix_web::patch("/v1/scenarios/{id}")]
async fn patch_scenario(
    path: web::Path<String>,
    scenario: web::Json<Scenario>,
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

    match scenario_index {
        Some(index) => {
            loaded_scenarios.scenarios[index] = scenario.into_inner();
            let response = serde_json::json!({"id": scenario_id});
            Ok(HttpResponse::Ok()
                .content_type("application/json")
                .body(response.to_string()))
        }
        None => {
            loaded_scenarios.scenarios.push(scenario.into_inner());
            let response = serde_json::json!({"id": scenario_id});
            Ok(HttpResponse::Ok()
                .content_type("application/json")
                .body(response.to_string()))
        }
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
