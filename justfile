# RabbitControl macOS 常用开发指令

default:
    @just --list

c crate:
    cargo check -p {{crate}}

ct crate:
    cargo check -p {{crate}} --tests

t crate:
    cargo test -p {{crate}}

cw:
    cargo check --workspace

ta:
    cargo test --workspace -- --test-threads=1

f:
    cargo fmt --all
    cargo clippy --workspace --all-targets -- -D warnings

v: f ta
    @echo "✓ all checks passed"

clean-disk:
    ./scripts/clean-disk.sh

clean:
    cargo clean

dev-cert:
    ./scripts/macos-dev-run.sh check-cert

dev-probe:
    ./scripts/macos-dev-run.sh cli -- probe --json

dev-cli *args:
    ./scripts/macos-dev-run.sh cli {{args}}

dev-test crate *args:
    ./scripts/macos-dev-run.sh test {{crate}} {{args}}

dev-ui *args:
    ./scripts/macos-dev-run.sh ui {{args}}

package-macos *args:
    ./scripts/package-macos.sh {{args}}

ui:
    cargo run -p rc-ui

ui-preview port="8791":
    cd crates/rc-ui/ui && python3 -m http.server {{port}}
