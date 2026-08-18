//! Opt-in real-provider smoke cases for the core chain.
//!
//! These tests are ignored by default. They require an explicit temporary
//! models configuration and the selected provider's key in the process
//! environment; they never read the user's normal home/configuration.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::{Builder, TempDir};

struct SmokeFixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    models_config: PathBuf,
    app_server: PathBuf,
}

impl SmokeFixture {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sg"));
        command
            .current_dir(&self.workspace)
            .env("SINGULARITY_HOME", &self.home)
            .env("SINGULARITY_APP_SERVER_DB", self.home.join("index.sqlite3"))
            .env("SINGULARITY_MODELS_CONFIG", &self.models_config)
            .env("SINGULARITY_APP_SERVER_BIN", &self.app_server);
        command
    }
}

fn fixture() -> Result<SmokeFixture, String> {
    let source = env::var_os("SINGULARITY_SMOKE_MODELS_CONFIG")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "set SINGULARITY_SMOKE_MODELS_CONFIG to a temporary provider config".to_string()
        })?;
    if !source.is_absolute() {
        return Err("SINGULARITY_SMOKE_MODELS_CONFIG must be an absolute path".to_string());
    }
    let source_text = fs::read_to_string(&source)
        .map_err(|_| "SINGULARITY_SMOKE_MODELS_CONFIG could not be read".to_string())?;
    let source_json: Value = serde_json::from_str(&source_text)
        .map_err(|_| "SINGULARITY_SMOKE_MODELS_CONFIG is not valid JSON".to_string())?;
    let default_model = source_json
        .get("default_model")
        .and_then(Value::as_str)
        .ok_or_else(|| "smoke models config must declare default_model".to_string())?;
    let provider_name = default_model
        .split_once('/')
        .map(|(provider, _)| provider)
        .ok_or_else(|| "smoke default_model must use provider/model form".to_string())?;
    let api_key_env = source_json
        .pointer(&format!("/providers/{provider_name}/api_key_env"))
        .and_then(Value::as_str)
        .ok_or_else(|| "smoke default provider must declare api_key_env".to_string())?;
    if env::var_os(api_key_env).is_none() {
        return Err("the selected provider key environment variable is missing".to_string());
    }

    let root = isolated_tempdir()?;
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    let models_config = root.path().join("models.json");
    fs::create_dir_all(&home).map_err(|_| "could not create smoke home".to_string())?;
    fs::create_dir_all(&workspace).map_err(|_| "could not create smoke workspace".to_string())?;
    fs::write(&models_config, source_text)
        .map_err(|_| "could not copy smoke models config".to_string())?;

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
        models_config,
        app_server,
    })
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

fn parse_success_json(output: Output) -> Result<Value, String> {
    if !output.status.success() {
        return Err(format!(
            "sg run exited with status {:?}",
            output.status.code()
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

#[test]
#[ignore = "requires explicit temporary provider config and credentials"]
fn real_provider_restart_and_resume_smoke() {
    let fixture = fixture().expect("explicit isolated smoke configuration");
    let first = run_json(&fixture, "Reply with a short acknowledgement.").expect("first turn");
    let id = thread_id(&first).expect("thread id").to_string();
    run_continue(&fixture, &id, "Reply with one more short acknowledgement.")
        .expect("cross-process resume");
    assert!(fixture.home.join("index.sqlite3").is_file());
    assert!(session_file(&fixture).is_ok());
}

#[test]
#[ignore = "requires explicit temporary provider config and credentials"]
fn real_provider_compaction_smoke() {
    let fixture = fixture().expect("explicit isolated smoke configuration");
    let long_goal = "Keep the following context in mind and answer briefly: ".to_string()
        + &"context ".repeat(20_000);
    let first = run_json(&fixture, &long_goal).expect("initial long turn");
    let id = thread_id(&first).expect("thread id").to_string();
    run_continue(&fixture, &id, &long_goal).expect("compaction continuation");
    let rollout = fs::read_to_string(session_file(&fixture).expect("session file"))
        .expect("read isolated rollout");
    assert!(
        rollout.contains("compaction"),
        "the long continuation did not compact"
    );
}

#[test]
#[ignore = "requires explicit temporary provider config and credentials"]
fn real_provider_tool_and_parallel_recovery_smoke() {
    let fixture = fixture().expect("explicit isolated smoke configuration");
    let first = run_json(
        &fixture,
        "Use independent write tools to create smoke-a.txt containing A and smoke-b.txt containing B, then report both paths.",
    )
    .expect("tool turn");
    let id = thread_id(&first).expect("thread id").to_string();
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
