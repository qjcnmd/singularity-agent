use crate::wfp::install_wfp_filters_for_account;
use anyhow::Result;

fn panic_payload_to_string(panic_payload: Box<dyn std::any::Any + Send>) -> String {
    match panic_payload.downcast::<String>() {
        Ok(message) => *message,
        Err(panic_payload) => match panic_payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "unknown panic payload".to_string(),
        },
    }
}

/// Installs the persistent WFP filters for the offline identity and grants the
/// invoking operator read-only access needed for runtime drift checks.
///
/// Network-restricted execution is fail-closed: setup returns an error when WFP
/// installation fails or panics instead of recording an optimistic setup state.
pub fn install_wfp_filters<F>(
    offline_username: &str,
    reader_account: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
    mut log: F,
) -> Result<usize>
where
    F: FnMut(&str),
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        install_wfp_filters_for_account(
            offline_username,
            reader_account,
            proxy_ports,
            allow_local_binding,
        )
    })) {
        Ok(Ok(installed_filter_count)) => {
            log(&format!(
                "WFP setup succeeded for {offline_username} with {installed_filter_count} installed filters"
            ));
            Ok(installed_filter_count)
        }
        Ok(Err(error)) => {
            log(&format!("WFP setup failed for {offline_username}: {error}"));
            Err(error)
        }
        Err(panic_payload) => {
            let error = panic_payload_to_string(panic_payload);
            log(&format!(
                "WFP setup panicked for {offline_username}: {error}"
            ));
            anyhow::bail!("WFP setup panicked for {offline_username}: {error}")
        }
    }
}
