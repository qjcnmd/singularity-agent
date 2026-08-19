//! Opt-in real-provider smoke cases for the core chain.
//!
//! These tests are ignored by default. They copy the selected persistent user
//! configuration and its private auth generation into a temporary home before
//! starting `sg`; the source files are never modified or logged.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::{Builder, TempDir};

struct SmokeFixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    config_path: PathBuf,
    app_server: PathBuf,
}

#[derive(Clone, Copy)]
enum SmokeScenario {
    LongCat,
    ResponsesReplay,
}

impl SmokeFixture {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sg"));
        command
            .current_dir(&self.workspace)
            .env("SINGULARITY_HOME", &self.home)
            .env("SINGULARITY_APP_SERVER_DB", self.home.join("index.sqlite3"))
            .env("SINGULARITY_APP_SERVER_BIN", &self.app_server)
            .env_remove("SINGULARITY_MODELS_CONFIG")
            .env_remove("SINGULARITY_MODEL_PROVIDER")
            .env_remove("SINGULARITY_MODEL")
            .env_remove("SINGULARITY_MODEL_CONTEXT_TOKENS")
            .env_remove("SINGULARITY_MODEL_MAX_OUTPUT_TOKENS")
            .env_remove("SINGULARITY_BASE_URL")
            .env_remove("SINGULARITY_API_KEY");
        command
    }
}

fn fixture(scenario: SmokeScenario) -> Result<SmokeFixture, String> {
    let source_home = smoke_source_home()?;
    let source_config = source_home.join("config.json");
    let mut config: Value = serde_json::from_str(
        &fs::read_to_string(&source_config)
            .map_err(|_| "persistent smoke config could not be read".to_string())?,
    )
    .map_err(|_| "persistent smoke config is not valid JSON".to_string())?;
    let auth_generation = config
        .get("auth_generation")
        .and_then(Value::as_str)
        .ok_or_else(|| "persistent smoke config must declare auth_generation".to_string())?
        .to_string();
    let source_auth = auth_generation_path(&source_home, &auth_generation)?;
    if !source_auth.is_file() {
        return Err("persistent smoke auth generation is unavailable".to_string());
    }
    let selector = match scenario {
        SmokeScenario::LongCat => select_longcat_selector(&config)?,
        SmokeScenario::ResponsesReplay => select_responses_replay_selector(&config)?,
    };
    set_default_selector(&mut config, &selector)?;
    let root = isolated_tempdir()?;
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&home).map_err(|_| "could not create smoke home".to_string())?;
    fs::create_dir_all(&workspace).map_err(|_| "could not create smoke workspace".to_string())?;
    let config_path = home.join("config.json");
    fs::write(
        &config_path,
        serde_json::to_vec(&config).map_err(|_| "could not serialize smoke config".to_string())?,
    )
    .map_err(|_| "could not copy smoke config".to_string())?;
    let isolated_auth = home.join(&auth_generation);
    fs::copy(&source_auth, &isolated_auth)
        .map_err(|_| "could not copy smoke auth generation".to_string())?;
    set_owner_only_permissions(&config_path)?;
    set_owner_only_permissions(&isolated_auth)?;

    let app_server = env::var_os("SINGULARITY_APP_SERVER_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            let cli = PathBuf::from(env!("CARGO_BIN_EXE_sg"));
            let name = if cfg!(windows) {
                "singularity_app_server.exe"
            } else {
                "singularity_app_server"
            };
            cli.parent().map(|parent| parent.join(name))
        })
        .ok_or_else(|| "could not locate singularity_app_server".to_string())?;
    if !app_server.is_file() {
        return Err("singularity_app_server binary is unavailable".to_string());
    }

    Ok(SmokeFixture {
        _root: root,
        home,
        workspace,
        config_path,
        app_server,
    })
}

fn smoke_source_home() -> Result<PathBuf, String> {
    let source = env::var_os("SINGULARITY_SMOKE_SOURCE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("USERPROFILE")
                .or_else(|| env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".singularity"))
        })
        .ok_or_else(|| "persistent Singularity home is unavailable".to_string())?;
    if !source.is_absolute() {
        return Err("persistent smoke home must be an absolute path".to_string());
    }
    Ok(source)
}

fn auth_generation_path(home: &Path, generation: &str) -> Result<PathBuf, String> {
    let path = Path::new(generation);
    if !generation.starts_with("auth.v1-")
        || !generation.ends_with(".json")
        || generation.contains(['/', '\\', ':'])
        || path.is_absolute()
        || path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(generation)
    {
        return Err("persistent smoke auth_generation is invalid".to_string());
    }
    Ok(home.join(path))
}

fn set_default_selector(config: &mut Value, selector: &str) -> Result<(), String> {
    let (provider, _) = selector
        .split_once('/')
        .ok_or_else(|| "smoke model selector must use provider/model form".to_string())?;
    let root = config
        .as_object_mut()
        .ok_or_else(|| "persistent smoke config must be an object".to_string())?;
    root.insert(
        "default_provider".to_string(),
        Value::String(provider.to_string()),
    );
    root.insert(
        "default_model".to_string(),
        Value::String(selector.to_string()),
    );
    Ok(())
}

fn select_longcat_selector(config: &Value) -> Result<String, String> {
    selector_with_highest_reasoning(config, "longcat", "LongCat-2.0")
}

fn select_responses_replay_selector(config: &Value) -> Result<String, String> {
    let providers = config
        .get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| "persistent smoke config must declare providers".to_string())?;
    let mut selected = Vec::new();
    for (provider_name, provider) in providers {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_name, model) in models {
            if model.get("api_protocol").and_then(Value::as_str) == Some("responses")
                && model.get("tool_reasoning_history").and_then(Value::as_str)
                    == Some("responses_items")
            {
                selected.push(selector_with_highest_reasoning(
                    config,
                    provider_name,
                    model_name,
                )?);
            }
        }
    }
    match selected.as_slice() {
        [selector] => Ok(selector.clone()),
        [] => Err("persistent smoke config has no Responses private-replay model".to_string()),
        _ => {
            Err("persistent smoke config has multiple Responses private-replay models".to_string())
        }
    }
}

fn selector_with_highest_reasoning(
    config: &Value,
    provider_name: &str,
    model_name: &str,
) -> Result<String, String> {
    let model = config
        .pointer(&format!("/providers/{provider_name}/models/{model_name}"))
        .ok_or_else(|| "persistent smoke config is missing the required model".to_string())?;
    let variant = highest_enabled_reasoning_variant(model)?;
    Ok(format!("{provider_name}/{model_name}#{variant}"))
}

fn highest_enabled_reasoning_variant(model: &Value) -> Result<&str, String> {
    const ORDER: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];
    let variants = model
        .get("reasoning_variants")
        .and_then(Value::as_object)
        .ok_or_else(|| "smoke model must declare reasoning_variants".to_string())?;
    ORDER
        .into_iter()
        .rev()
        .find(|variant| {
            variants
                .get(*variant)
                .and_then(|value| value.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true)
        })
        .ok_or_else(|| "smoke model has no enabled ranked reasoning variant".to_string())
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "could not protect isolated smoke credentials".to_string())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn require_responses_replay_contract(fixture: &SmokeFixture) -> Result<(), String> {
    let config: Value = serde_json::from_str(
        &fs::read_to_string(&fixture.config_path)
            .map_err(|_| "could not read isolated smoke config".to_string())?,
    )
    .map_err(|_| "isolated smoke config is not valid JSON".to_string())?;
    let default_model = config
        .get("default_model")
        .and_then(Value::as_str)
        .ok_or_else(|| "smoke config does not declare default_model".to_string())?;
    let (provider_name, selected) = default_model
        .split_once('/')
        .ok_or_else(|| "smoke default_model must use provider/model form".to_string())?;
    let (model_name, requested_variant) = selected
        .split_once('#')
        .map_or((selected, None), |(model, variant)| (model, Some(variant)));
    let model = config
        .pointer(&format!("/providers/{provider_name}/models/{model_name}"))
        .ok_or_else(|| "smoke config does not contain the selected model".to_string())?;
    if model.get("api_protocol").and_then(Value::as_str) != Some("responses") {
        return Err("restart smoke requires api_protocol=responses".to_string());
    }
    if model.get("tool_reasoning_history").and_then(Value::as_str) != Some("responses_items") {
        return Err("restart smoke requires tool_reasoning_history=responses_items".to_string());
    }
    let variants = model
        .get("reasoning_variants")
        .and_then(Value::as_object)
        .ok_or_else(|| "restart smoke requires explicit reasoning_variants".to_string())?;
    let variant_name = requested_variant
        .or_else(|| model.get("default_variant").and_then(Value::as_str))
        .ok_or_else(|| "restart smoke requires an explicit reasoning variant".to_string())?;
    let variant = variants
        .get(variant_name)
        .ok_or_else(|| "restart smoke selected reasoning variant is not declared".to_string())?;
    if variant.get("enabled").and_then(Value::as_bool) != Some(true) {
        return Err("restart smoke requires an enabled reasoning variant".to_string());
    }
    Ok(())
}

fn isolated_tempdir() -> Result<TempDir, String> {
    for parent in env::temp_dir().ancestors() {
        if parent.join(".git").exists() {
            continue;
        }
        if let Ok(dir) = Builder::new()
            .prefix("singularity-core-smoke-")
            .tempdir_in(parent)
        {
            return Ok(dir);
        }
    }
    Err("could not create a temporary directory outside the repository".to_string())
}

fn run_json(fixture: &SmokeFixture, goal: &str) -> Result<Value, String> {
    let output = fixture
        .command()
        .args(["run", "--json", goal])
        .output()
        .map_err(|_| "could not start sg run".to_string())?;
    parse_success_json(output)
}

fn run_continue(fixture: &SmokeFixture, thread_id: &str, instruction: &str) -> Result<(), String> {
    let output = fixture
        .command()
        .args(["continue", thread_id, instruction])
        .output()
        .map_err(|_| "could not start sg continue".to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sg continue exited with status {:?}",
            output.status.code()
        ))
    }
}

fn configure_compaction_fixture(fixture: &SmokeFixture) -> Result<(), String> {
    let mut config: Value = serde_json::from_str(
        &fs::read_to_string(&fixture.config_path)
            .map_err(|_| "could not read isolated smoke config".to_string())?,
    )
    .map_err(|_| "isolated smoke config is not valid JSON".to_string())?;
    let default_model = config
        .get("default_model")
        .and_then(Value::as_str)
        .ok_or_else(|| "smoke config does not declare default_model".to_string())?;
    let (provider_name, model_selector) = default_model
        .split_once('/')
        .ok_or_else(|| "smoke default_model must use provider/model form".to_string())?;
    let provider_name = provider_name.to_string();
    let model_name = model_selector
        .split_once('#')
        .map_or(model_selector, |(name, _)| name)
        .to_string();
    let model = config
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .and_then(|providers| providers.get_mut(&provider_name))
        .and_then(|provider| provider.get_mut("models"))
        .and_then(Value::as_object_mut)
        .and_then(|models| models.get_mut(&model_name))
        .ok_or_else(|| "smoke config does not contain the selected model".to_string())?;
    // The isolated project instructions plus the normal request envelope need
    // more than 26k tokens even after compaction. Two bounded tool reads
    // exceed this window's 15,616-token trigger threshold, while the compacted
    // follow-up request still fits.
    model["max_context_tokens"] = Value::from(32_768_u64);
    model["max_output_tokens"] = Value::from(1_024_u64);
    if let Some(capabilities) = model.get_mut("capabilities") {
        capabilities["max_context_tokens"] = Value::from(32_768_u64);
        capabilities["max_output_tokens"] = Value::from(1_024_u64);
    }
    fs::write(
        &fixture.config_path,
        serde_json::to_vec(&config).map_err(|_| "could not serialize smoke config".to_string())?,
    )
    .map_err(|_| "could not update isolated compaction smoke config".to_string())?;
    set_owner_only_permissions(&fixture.config_path)?;
    fs::write(
        fixture.workspace.join("AGENTS.md"),
        // Stay below the per-file 32 KiB project-instruction limit while
        // making a full tool read large enough to trigger compaction.
        "Keep this context available for the task. ".repeat(780),
    )
    .map_err(|_| "could not create compaction smoke context".to_string())
}

fn parse_success_json(output: Output) -> Result<Value, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "sg run exited with status {:?}: {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "sg run did not return JSON".to_string())
}

fn thread_id(result: &Value) -> Result<&str, String> {
    result
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "smoke response did not include a thread id".to_string())
}

fn session_file(fixture: &SmokeFixture) -> Result<PathBuf, String> {
    let mut files = fs::read_dir(fixture.home.join("sessions"))
        .map_err(|_| "smoke session directory is unavailable".to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        });
    files
        .next()
        .ok_or_else(|| "smoke did not create a session JSONL".to_string())
}

fn session_entries(fixture: &SmokeFixture) -> Result<Vec<Value>, String> {
    let rollout = fs::read_to_string(session_file(fixture)?)
        .map_err(|_| "could not read isolated rollout".to_string())?;
    rollout
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .map_err(|_| "rollout contains invalid JSON".to_string())
        })
        .collect()
}

fn session_contains_responses_replay(fixture: &SmokeFixture) -> Result<bool, String> {
    Ok(session_entries(fixture)?.iter().any(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("message")
            && entry.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
            && entry
                .pointer("/message/providerReasoningReplay/protocol")
                .and_then(Value::as_str)
                == Some("responses")
            && entry
                .pointer("/message/providerReasoningReplay/items")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                })
    }))
}

fn session_contains_multi_tool_assistant(fixture: &SmokeFixture) -> Result<bool, String> {
    Ok(session_entries(fixture)?.iter().any(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("message")
            && entry.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
            && entry
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_call")
                        })
                        .count()
                        >= 2
                })
    }))
}

fn session_contains_compaction(fixture: &SmokeFixture) -> Result<bool, String> {
    let rollout = fs::read_to_string(session_file(fixture)?)
        .map_err(|_| "could not read isolated rollout".to_string())?;
    Ok(rollout.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|entry| entry.get("type").and_then(Value::as_str).map(str::to_owned))
            .as_deref()
            == Some("compaction")
    }))
}

fn session_entry_shapes(fixture: &SmokeFixture) -> Result<String, String> {
    let rollout = fs::read_to_string(session_file(fixture)?)
        .map_err(|_| "could not read isolated rollout".to_string())?;
    Ok(rollout
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line).ok().map(|entry| {
                let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("?");
                let role = entry
                    .pointer("/message/role")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                format!("{entry_type}:{role}:{}B", line.len())
            })
        })
        .collect::<Vec<_>>()
        .join("; "))
}

#[test]
#[ignore = "requires persistent Singularity config and credentials"]
fn real_provider_restart_and_resume_smoke() {
    let fixture = fixture(SmokeScenario::ResponsesReplay).expect("isolated smoke configuration");
    require_responses_replay_contract(&fixture).expect("explicit Responses replay configuration");
    let first = run_json(
        &fixture,
        "Use one write tool call to create responses-restart.txt containing restart, then reply with a short acknowledgement.",
    )
    .expect("first turn");
    let id = thread_id(&first).expect("thread id").to_string();
    assert!(
        session_contains_responses_replay(&fixture).expect("read Responses replay"),
        "restart smoke must persist a Responses provider-private replay"
    );
    run_continue(&fixture, &id, "Reply with one more short acknowledgement.")
        .expect("cross-process resume");
    assert!(fixture.home.join("index.sqlite3").is_file());
    assert!(session_file(&fixture).is_ok());
}

#[test]
#[ignore = "requires persistent Singularity config and credentials"]
fn real_provider_compaction_smoke() {
    let fixture = fixture(SmokeScenario::LongCat).expect("isolated smoke configuration");
    configure_compaction_fixture(&fixture).expect("compaction fixture");
    let first = run_json(
        &fixture,
        "Use the read tool to read every line of AGENTS.md, then reply with a short acknowledgement.",
    )
    .expect("initial compaction turn");
    let id = thread_id(&first).expect("thread id").to_string();
    let mut compacted = false;
    for _ in 0..3 {
        run_continue(
            &fixture,
            &id,
            "Use the read tool to reread every line of AGENTS.md, then reply with a short acknowledgement.",
        )
        .expect("compaction continuation");
        if session_contains_compaction(&fixture).expect("read compaction rollout") {
            compacted = true;
            break;
        }
    }
    assert!(
        compacted,
        "the bounded compaction continuations did not compact; entry shapes: {}",
        session_entry_shapes(&fixture).expect("read compaction entry shapes")
    );
    run_continue(
        &fixture,
        &id,
        "Confirm that the compacted project context remains available.",
    )
    .expect("continuation after compaction");
}

#[test]
#[ignore = "requires persistent Singularity config and credentials"]
fn real_provider_tool_and_parallel_recovery_smoke() {
    let fixture = fixture(SmokeScenario::LongCat).expect("isolated smoke configuration");
    let first = run_json(
        &fixture,
        "In one response, use two independent write tool calls: create smoke-a.txt containing A and smoke-b.txt containing B. Do not split the writes across responses, then report both paths.",
    )
    .expect("tool turn");
    let id = thread_id(&first).expect("thread id").to_string();
    assert!(
        session_contains_multi_tool_assistant(&fixture).expect("read assistant tool calls"),
        "parallel smoke must persist one assistant entry containing at least two tool calls"
    );
    run_continue(
        &fixture,
        &id,
        "Confirm the two files still exist after reconnecting.",
    )
    .expect("tool recovery continuation");
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("smoke-a.txt"))
            .ok()
            .as_deref(),
        Some("A")
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("smoke-b.txt"))
            .ok()
            .as_deref(),
        Some("B")
    );
}

#[test]
fn longcat_selector_uses_the_highest_enabled_reasoning_variant() {
    let config = json!({
        "providers": {
            "longcat": {
                "models": {
                    "LongCat-2.0": {
                        "reasoning_variants": {
                            "low": { "enabled": true },
                            "high": { "enabled": true },
                            "max": { "enabled": false }
                        }
                    }
                }
            }
        }
    });

    assert_eq!(
        select_longcat_selector(&config).expect("LongCat selector"),
        "longcat/LongCat-2.0#high"
    );
}

#[test]
fn responses_replay_selector_requires_one_matching_model_and_uses_its_highest_variant() {
    let config = json!({
        "providers": {
            "chat": {
                "models": {
                    "ordinary": {
                        "api_protocol": "chat",
                        "tool_reasoning_history": "reasoning_content",
                        "reasoning_variants": { "high": { "enabled": true } }
                    }
                }
            },
            "responses": {
                "models": {
                    "private": {
                        "api_protocol": "responses",
                        "tool_reasoning_history": "responses_items",
                        "reasoning_variants": {
                            "high": { "enabled": true },
                            "max": { "enabled": true }
                        }
                    }
                }
            }
        }
    });

    assert_eq!(
        select_responses_replay_selector(&config).expect("Responses replay selector"),
        "responses/private#max"
    );
}

#[test]
fn responses_replay_selector_fails_closed_without_one_matching_model() {
    let no_match = json!({
        "providers": {
            "chat": {
                "models": {
                    "ordinary": {
                        "api_protocol": "chat",
                        "tool_reasoning_history": "reasoning_content",
                        "reasoning_variants": { "high": { "enabled": true } }
                    }
                }
            }
        }
    });
    let multiple_matches = json!({
        "providers": {
            "responses": {
                "models": {
                    "one": {
                        "api_protocol": "responses",
                        "tool_reasoning_history": "responses_items",
                        "reasoning_variants": { "high": { "enabled": true } }
                    },
                    "two": {
                        "api_protocol": "responses",
                        "tool_reasoning_history": "responses_items",
                        "reasoning_variants": { "high": { "enabled": true } }
                    }
                }
            }
        }
    });

    assert!(select_responses_replay_selector(&no_match).is_err());
    assert!(select_responses_replay_selector(&multiple_matches).is_err());
}

#[test]
fn auth_generation_is_limited_to_a_single_expected_filename() {
    let home = Path::new("C:/smoke-home");
    assert_eq!(
        auth_generation_path(home, "auth.v1-abc.json").expect("safe filename"),
        home.join("auth.v1-abc.json")
    );
    for invalid in [
        "../auth.v1-abc.json",
        "auth.v1-abc.txt",
        "other.json",
        "auth.v1-a/b.json",
    ] {
        assert!(auth_generation_path(home, invalid).is_err(), "{invalid}");
    }
}
