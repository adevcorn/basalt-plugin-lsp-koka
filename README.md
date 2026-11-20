# lsp-koka

Koka language-server bridge for Basalt (`CAP_DIAGNOSTICS | CAP_HOVER`).

Wraps `koka --language-server --lsstdio` via Basalt's host LSP process imports for `.kk` and `.kki` files.

## Build

```sh
cargo build --release --target wasm32-unknown-unknown
```
