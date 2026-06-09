# Shiku — deploy platform for native Rust services on a Linux box.

# Check the whole workspace.
check:
    cargo check -p shiku -p shikud -p shiku-types

# Clippy, warnings as errors.
lint:
    cargo clippy -p shiku -p shikud -p shiku-types --no-deps -- -D warnings

# Run the test suite.
test:
    cargo test -p shiku -p shikud -p shiku-types

# Run the CLI locally (forwards args after --, e.g. `just shiku -- ping`).
shiku *args:
    cargo run -p shiku -- {{ args }}

# Cross-compile the CLI + agent for an arm64 Linux box.
build-arm:
    cargo zigbuild --release --target aarch64-unknown-linux-musl -p shiku -p shikud
