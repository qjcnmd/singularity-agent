//! CLI command orchestration.

use super::*;
use crate::client::app_server_bin;
use singularity_model::{
    add_configured_provider, discover_provider_model_ids, refresh_model_metadata,
};
use singularity_protocol::ProviderConfigurationStatus;

fn print_readiness() -> Result<(), String> {
    let mut client = AppServerClient::spawn()?;
    client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
    client.initialize()?;
    println!("agent_loop=available (headless core)");
    let status = client.provider_status()?;
    print_provider_configuration(&status)
}

fn print_provider_configuration(provider: &ProviderConfigurationStatus) -> Result<(), String> {
    let source = match provider.source.as_deref() {
        Some("process_env") => "process_env",
        Some("user_config") => "user_config",
        None => "unconfigured",
        _ => return Err("invalid provider status: providerConfiguration.source".to_string()),
    };
    println!("provider_config_source={source}");
    let snapshot_id = (!provider.snapshot_id.trim().is_empty())
        .then_some(provider.snapshot_id.as_str())
        .ok_or_else(|| "invalid provider status: providerConfiguration.snapshotId".to_string())?;
    println!("provider_snapshot_id={snapshot_id}");
    println!("provider_configured={}", provider.configured);
    let blocker = match provider.configuration_blocker.as_deref() {
        None => "none",
        Some(blocker) if !blocker.trim().is_empty() => blocker,
        _ => {
            return Err(
                "invalid provider status: providerConfiguration.configurationBlocker".to_string(),
            );
        }
    };
    println!("provider_configuration_blocker={blocker}");
    for (name, present) in [
        ("SINGULARITY_API_KEY", provider.api_key_present),
        ("SINGULARITY_BASE_URL", provider.base_url_present),
        ("SINGULARITY_MODEL", provider.model_present),
    ] {
        println!(
            "{name}={}",
            if present {
                "present(redacted)"
            } else {
                "missing"
            }
        );
    }
    Ok(())
}

fn print_json_notification(notification: &singularity_protocol::JsonRpcNotification) {
    println!(
        "{}",
        serde_json::to_string(notification).expect("notification serializes to JSON")
    );
}

fn print_json_summary(thread: serde_json::Value, turn: serde_json::Value) {
    println!(
        "{}",
        json!({ "summary": { "thread": thread, "turn": turn } })
    );
}

// 按命令编排 app-server 请求和面向用户的输出。
pub(super) fn run_cli(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Run {
            goal,
            model,
            session_reference,
            json,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            let goal = prepare_goal_with_session_reference(
                &mut client,
                &goal,
                session_reference.as_deref(),
            )?;
            // JSON 模式：事件逐行 JSONL 输出（notification 原样），终态汇总行收尾。
            let mut on_notification = |notification: &singularity_protocol::JsonRpcNotification| {
                if json {
                    print_json_notification(notification);
                }
            };
            let thread = match client.thread_start(model, !json, &mut on_notification) {
                Ok(thread) => thread,
                Err(error) => {
                    if json {
                        print_json_summary(
                            json!(null),
                            json!({"threadId": null, "turnId": null, "status": "failed"}),
                        );
                    }
                    return Err(error);
                }
            };
            if !json {
                println!("thread {}", thread.thread_id);
            }
            let turn = match client.turn_start(
                &thread.thread_id,
                &goal,
                !json,
                &mut on_notification,
            ) {
                Ok(turn) => turn,
                Err(error) => {
                    if json {
                        print_json_summary(
                            json!(thread),
                            json!({"threadId": thread.thread_id, "turnId": null, "status": "failed"}),
                        );
                    }
                    return Err(error);
                }
            };
            if json {
                print_json_summary(json!(thread), json!(turn));
            }
            fail_for_failed_turn(&turn)?;
            Ok(())
        }
        Command::Continue {
            thread_id,
            instruction,
            model,
            json,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            client.thread_settings(&thread_id, model)?;
            if !json {
                println!("thread {thread_id}");
            }
            let mut on_notification = |notification: &singularity_protocol::JsonRpcNotification| {
                if json {
                    print_json_notification(notification);
                }
            };
            let turn =
                match client.turn_start(&thread_id, &instruction, !json, &mut on_notification) {
                    Ok(turn) => turn,
                    Err(error) => {
                        if json {
                            print_json_summary(
                                json!({"threadId": thread_id}),
                                json!({"threadId": thread_id, "turnId": null, "status": "failed"}),
                            );
                        }
                        return Err(error);
                    }
                };
            if json {
                print_json_summary(json!({"threadId": thread_id}), json!(turn));
            }
            fail_for_failed_turn(&turn)?;
            Ok(())
        }
        Command::Session { command } => match command {
            SessionCommand::Read { session_id, limit } => {
                let mut client = AppServerClient::spawn()?;
                client.initialize()?;
                client.session_read(&session_id, limit)?;
                Ok(())
            }
            SessionCommand::Delete { session_id } => {
                let mut client = AppServerClient::spawn()?;
                client.initialize()?;
                client.session_delete(&session_id)?;
                Ok(())
            }
        },
        Command::Threads => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.thread_list()?;
            Ok(())
        }
        Command::Config { command } => match command {
            ConfigCommand::Doctor => {
                println!("app_server_bin={}", app_server_bin()?);
                println!("client=protocol-only");
                print_readiness()?;
                Ok(())
            }
            ConfigCommand::Models { refresh } => {
                let catalog = read_user_model_catalog(refresh)
                    .map_err(|error| format!("failed to read provider models: {error}"))?;
                println!(
                    "default_selector={}",
                    catalog.default_selector.as_deref().unwrap_or("none")
                );
                println!("cache_status={:?}", catalog.cache_status);
                if catalog.providers.is_empty() {
                    println!("providers=none");
                }
                for provider in catalog.providers {
                    println!(
                        "provider={} discovery={:?} api_key={} base_url={}",
                        provider.provider_name,
                        provider.discovery,
                        if provider.api_key_present {
                            "present(redacted)"
                        } else {
                            "missing"
                        },
                        if provider.base_url_present {
                            "present(redacted)"
                        } else {
                            "missing"
                        }
                    );
                    for model in provider.models {
                        println!(
                            "model={} discovered={} explicit={} selectable={} variants={} default_variant={}",
                            model.id,
                            model.discovered,
                            model.explicit,
                            model.selectable,
                            if model.reasoning_variants.is_empty() {
                                "none".to_string()
                            } else {
                                model.reasoning_variants.join(",")
                            },
                            model.default_variant.as_deref().unwrap_or("none")
                        );
                    }
                    if let Some(error) = provider.error {
                        println!("provider_error={error}");
                    }
                }
                Ok(())
            }
            ConfigCommand::ImportEnv { file } => {
                let result = import_env_to_user_config(file.as_deref())
                    .map_err(|error| format!("failed to import provider env: {error}"))?;
                println!("config_path={}", result.config_path);
                println!("auth_path={}", result.auth_path);
                println!("provider={}", result.provider_name);
                println!(
                    "default_selector={}",
                    result.default_selector.as_deref().unwrap_or("none")
                );
                println!("selectable={}", result.selectable);
                Ok(())
            }
            ConfigCommand::Add {
                name,
                base_url,
                api_key,
            } => {
                let api_key = api_key
                    .or_else(|| std::env::var("SINGULARITY_API_KEY").ok())
                    .ok_or_else(|| {
                        "an api key is required: pass --api-key or set SINGULARITY_API_KEY"
                            .to_string()
                    })?;
                refresh_model_metadata();
                let model_ids = discover_provider_model_ids(&base_url, &api_key)
                    .map_err(|error| format!("failed to discover provider models: {error}"))?;
                println!("discovered_models={}", model_ids.len());
                for model_id in &model_ids {
                    println!("discovered_model={model_id}");
                }
                let result = add_configured_provider(&name, &base_url, &api_key, model_ids)
                    .map_err(|error| format!("failed to persist provider config: {error}"))?;
                println!("config_path={}", result.config_path);
                println!("auth_path={}", result.auth_path);
                println!("provider={}", result.provider_name);
                println!(
                    "default_selector={}",
                    result.default_selector.as_deref().unwrap_or("none")
                );
                println!("models_written={}", result.models_written);
                Ok(())
            }
        },
    }
}
