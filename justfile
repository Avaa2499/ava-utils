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
    mkdir -p "$HOME/.local/share/bash-completion/completions"
    "$HOME/.local/bin/au" completions bash > "$HOME/.local/share/bash-completion/completions/au"
