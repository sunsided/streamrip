fn main() {
    let hls = std::env::var_os("CARGO_FEATURE_HLS").is_some();
    let dash = std::env::var_os("CARGO_FEATURE_DASH").is_some();

    if !hls && !dash {
        panic!(
            "\nerror: You must enable at least one feature: `hls` or `dash`.\n\
             Examples:\n\
               cargo build --features hls\n\
               cargo build --features dash\n\
               cargo build --features \"hls,dash\"\n"
        );
    }
}
