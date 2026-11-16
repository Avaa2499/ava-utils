check:
    cargo check --workspace --all-targets

test:
    cargo test --workspace

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

install:
    cargo build --release -p ava-utils-cli
    install -Dm755 target/release/au "$HOME/.local/bin/au"
