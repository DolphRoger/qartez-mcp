#![no_main]

use libfuzzer_sys::fuzz_target;
use qartez_mcp::graph::security::SecurityConfig;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _: Result<SecurityConfig, _> = toml_edit::de::from_str(text);
});
