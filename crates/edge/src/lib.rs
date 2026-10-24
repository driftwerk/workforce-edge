//! `workforce-edge` — the standalone customer-facing agent service.
//!
//! A separate binary and deployment from the internal hub. It hosts only the
//! customer chat surface and a cut-down admin UI, backed by its own Postgres
//! and its own Weaviate. It deliberately links none of the internal hub's
//! surface — no Slack, workflows, vault, teams, or internal admin routes — so
//! that neither internal downtime nor an edge-boundary bug can reach internal
//! data.

pub mod admin;
pub mod agent;
pub mod analytics;
pub mod assets;
pub mod boot;
pub mod build_info;
pub mod config;
pub mod config_store;
pub mod consolidation;
pub mod curator;
pub mod demo;
pub mod erasure;
pub mod export;
pub mod extract;
pub mod git_config;
pub mod grader;
pub mod host;
pub mod image_fetch;
pub mod ingress;
pub mod keycloak;
pub mod knowledge;
pub mod lessons;
pub mod mcp;
pub mod memory;
pub mod migrate;
pub mod oidc;
pub mod outbound;
pub mod passive_memory;
pub mod permissions;
pub mod platform;
pub mod playbook;
pub mod rich_output;
pub mod runtime;
pub mod server;
pub mod settings;
pub mod table_query;
pub mod tenant;
pub mod theme;
pub mod token_exchange;
pub mod upload;
pub mod version_tool;
pub mod wire;
