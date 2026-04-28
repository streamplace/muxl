# muxl — deterministic MP4 canonicalization

# Container CLI to use for Dockerfile-based builds. Defaults to `docker`,
# which works with podman via the podman-docker shim. Override with e.g.
# `DOCKER=podman just build-wasi-sign`.
docker := env("DOCKER", "docker")

# Default: list available recipes
default:
    @just --list

# Build the project
build:
    cargo build --workspace

# Build in release mode
build-release:
    cargo build --release --workspace

# Type-check without building
check:
    cargo check --workspace

# Run cargo tests
test: build
    cargo test --workspace

# Generate synthetic test fixtures (requires ffmpeg)
fixtures:
    bash scripts/generate-test-fixtures.sh

# Canonicalize a single file
canonicalize input output:
    cargo run --quiet -- canonicalize {{input}} {{output}}

# Fragment a file into per-frame CMAF
fragment input output_dir:
    cargo run --quiet -- fragment {{input}} {{output_dir}}

# Test canonicalization on all fixture files that we expect to work
test-canon: build fixtures
    #!/usr/bin/env bash
    set -euo pipefail
    pass=0; fail=0; skip=0
    tmpdir=$(mktemp -d)
    trap "rm -rf $tmpdir" EXIT
    for f in samples/fixtures/*.mp4; do
        name=$(basename "$f")
        # Known failures: AV1 (mp4-rust can't round-trip av01 stsd), fMP4 (fragment consolidation)
        case "$name" in
            av1-*|*-frag.mp4)
                skip=$((skip + 1))
                echo "SKIP $name (known mp4-rust limitation)"
                continue
                ;;
        esac
        if cargo run --quiet -- canonicalize "$f" "$tmpdir/$name" 2>/dev/null; then
            echo "OK   $name"
            pass=$((pass + 1))
        else
            echo "FAIL $name"
            fail=$((fail + 1))
        fi
    done
    echo ""
    echo "$pass passed, $fail failed, $skip skipped"
    [ "$fail" -eq 0 ]

# Test that canonicalization is idempotent (running twice gives identical bytes)
test-idempotent: build fixtures
    #!/usr/bin/env bash
    set -euo pipefail
    pass=0; fail=0; skip=0
    tmpdir=$(mktemp -d)
    trap "rm -rf $tmpdir" EXIT
    for f in samples/fixtures/*.mp4; do
        name=$(basename "$f")
        case "$name" in
            av1-*|*-frag.mp4)
                skip=$((skip + 1))
                continue
                ;;
        esac
        if ! cargo run --quiet -- canonicalize "$f" "$tmpdir/pass1-$name" 2>/dev/null; then
            continue
        fi
        if ! cargo run --quiet -- canonicalize "$tmpdir/pass1-$name" "$tmpdir/pass2-$name" 2>/dev/null; then
            echo "FAIL $name (2nd pass errored)"
            fail=$((fail + 1))
            continue
        fi
        h1=$(sha256sum "$tmpdir/pass1-$name" | cut -d' ' -f1)
        h2=$(sha256sum "$tmpdir/pass2-$name" | cut -d' ' -f1)
        if [ "$h1" = "$h2" ]; then
            echo "OK   $name"
            pass=$((pass + 1))
        else
            echo "FAIL $name (not idempotent)"
            fail=$((fail + 1))
        fi
    done
    echo ""
    echo "$pass passed, $fail failed, $skip skipped"
    [ "$fail" -eq 0 ]

# Test that the original sample file also works
test-sample: build
    #!/usr/bin/env bash
    set -euo pipefail
    tmpdir=$(mktemp -d)
    trap "rm -rf $tmpdir" EXIT
    cargo run --quiet -- canonicalize samples/file.mp4 "$tmpdir/pass1.mp4"
    cargo run --quiet -- canonicalize "$tmpdir/pass1.mp4" "$tmpdir/pass2.mp4"
    h1=$(sha256sum "$tmpdir/pass1.mp4" | cut -d' ' -f1)
    h2=$(sha256sum "$tmpdir/pass2.mp4" | cut -d' ' -f1)
    if [ "$h1" = "$h2" ]; then
        echo "OK   samples/file.mp4 (idempotent)"
    else
        echo "FAIL samples/file.mp4 (not idempotent)"
        exit 1
    fi

# Run all tests
test-all: test test-sample test-canon test-idempotent
    @echo "All tests passed."

# Dump flat box structure of a file (for diffing)
dump file:
    python3 scripts/mp4dump.py --flat {{file}}

# Diff two MP4 files at the box level
diff a b:
    diff <(python3 scripts/mp4dump.py --flat {{a}}) <(python3 scripts/mp4dump.py --flat {{b}})

# Show what canonicalization changes about a file
show-changes file: build
    #!/usr/bin/env bash
    set -euo pipefail
    tmpdir=$(mktemp -d)
    trap "rm -rf $tmpdir" EXIT
    cargo run --quiet -- canonicalize "{{file}}" "$tmpdir/canonical.mp4"
    diff <(python3 scripts/mp4dump.py --flat "{{file}}") \
         <(python3 scripts/mp4dump.py --flat "$tmpdir/canonical.mp4") || true

# Build WASI binary (for Go/wazero embedding)
build-wasi:
    cargo build --target wasm32-wasip1 --release

# Build the muxl-sign WASI binary inside a container.
# c2pa-rs pulls in `ring` which needs clang at compile time; running the
# build in a container lets you ship the .wasm without installing clang
# on the host. Output: target/wasm32-wasip1/release/muxl-sign.wasm
#
# Mounts the sibling ../c2pa-rs because of the temporary [patch] override
# in the workspace Cargo.toml (drop that mount once the patch goes away).
#
# Container runs as its default user (root on docker; mapped to the host
# user under rootless podman via the user namespace). On rootful docker
# the resulting target/ files end up root-owned — sudo chown if needed.
build-wasi-sign:
    {{docker}} build -q -t muxl-wasi-build -f Dockerfile.wasm .
    {{docker}} run --rm \
        -v "$(pwd)":/work \
        -v "$(pwd)/../c2pa-rs":/c2pa-rs \
        -e CARGO_HOME=/work/target/.docker-cargo \
        muxl-wasi-build \
        cargo build --release --target wasm32-wasip1 -p muxl-sign
    @echo "Built target/wasm32-wasip1/release/muxl-sign.wasm"

# Build browser WASM library (with wasm-bindgen)
build-wasm:
    cargo build --target wasm32-unknown-unknown --lib --features wasm --release

# Build all WASM targets
build-wasm-all: build-wasi build-wasm

# Clean build artifacts
clean:
    cargo clean
    rm -rf samples/fixtures/
