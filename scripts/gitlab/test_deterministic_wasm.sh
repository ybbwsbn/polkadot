#!/usr/bin/env bash

#shellcheck source=lib.sh
source "$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )/lib.sh"

skip_if_companion_pr

# build runtime
WASM_BUILD_NO_COLOR=1 cargo build --verbose --release -p kusama-runtime -p polkadot-runtime -p westend-runtime
# make checksum
sha256sum target/release/wbuild/target/wasm32-unknown-unknown/release/*.wasm > checksum.sha256
# clean up - FIXME: can we reuse some of the artifacts?
cargo clean
# build again
WASM_BUILD_NO_COLOR=1 cargo build --verbose --release -p kusama-runtime -p polkadot-runtime -p westend-runtime
# confirm checksum
sha256sum -c checksum.sha256
