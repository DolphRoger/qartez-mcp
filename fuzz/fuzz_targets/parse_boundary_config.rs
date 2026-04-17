#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = qartez_mcp::graph::boundaries::parse_config(text, Path::new("fuzz.toml"));
});
