# Building tern

tern needs Rust 1.99 or newer. Install it with [rustup](https://rustup.rs); distribution packages are usually older.

## macOS (Apple silicon)

```sh
git clone https://github.com/itskamol/tern && cd tern
scripts/make-local-identity.sh   # once: a stable local signature, so Keychain access survives updates
scripts/bundle-app.sh            # builds target/release/bundle/tern.app
cp -R target/release/bundle/tern.app /Applications/
cargo install --locked --path crates/tern   # the `tern` command (tern <host>, tern vault init)
```

## Linux (Debian/Ubuntu)

These are the packages CI installs on `ubuntu-latest` before `cargo build --release -p tern` (see `.github/workflows/build.yml`). Other distributions need the equivalents.

```sh
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential cmake clang pkg-config \
  libasound2-dev libfontconfig-dev libssl-dev libzstd-dev \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libx11-xcb-dev libxcb1-dev libxcb-xkb-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libvulkan-dev libdbus-1-dev
git clone https://github.com/itskamol/tern && cd tern
cargo build --release -p tern
```

## Windows

After installing a Rust toolchain targeting `x86_64-pc-windows-msvc`, the same `cargo build --release -p tern` produces the binary CI packages.

## Tests and the demo server

`cargo test --workspace` runs the suite, including SSH tests against an in-process server. `cargo run -p tern-ssh --example password_server -- --demo` starts the demo server used for the README screenshots.
