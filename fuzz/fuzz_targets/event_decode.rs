#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let _ = serde_json::from_value::<singularity_protocol::EventMetadata>(value.clone());
    let _ = serde_json::from_value::<singularity_protocol::EventGap>(value);
});
