//! Opt-in real-provider smoke cases for the core chain.
//!
//! These tests are ignored by default. They copy the selected persistent user
//! configuration and its private auth file into a temporary home before
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
}

#[derive(Clone, Copy)]
enum SmokeScenario {
    ResponsesReplay,
}

impl SmokeFixture {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sg"));
        command
            .current_dir(&self.workspace)
            .env("SINGULARITY_HOME", &self.home)
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
    let source_auth = source_home.join("auth.json");
    if !source_auth.is_file() {
        return Err("persistent smoke auth file is unavailable".to_string());
    }
    let selector = match scenario {
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
    let isolated_auth = home.join("auth.json");
    fs::copy(&source_auth, &isolated_auth)
        .map_err(|_| "could not copy smoke auth file".to_string())?;
    set_owner_only_permissions(&config_path)?;
    set_owner_only_permissions(&isolated_auth)?;

    Ok(SmokeFixture {
        _root: root,
        home,
        workspace,
        config_path,
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
        .args(["--json", goal])
        .output()
        .map_err(|_| "could not start sg --json".to_string())?;
    parse_success_json(output)
}

fn run_continue(fixture: &SmokeFixture, thread_id: &str, instruction: &str) -> Result<(), String> {
    let output = fixture
        .command()
        .args(["--json", "--session", thread_id, instruction])
        .output()
        .map_err(|_| "could not start sg --session".to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sg continue exited with status {:?}, stderr: {}, stdout: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ))
    }
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
        .pointer("/thread/threadId")
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
    assert!(session_file(&fixture).is_ok());
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
