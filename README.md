# authswap

`authswap` is a Rust command-line tool for switching local Codex auth profiles
and optionally syncing those profiles through WebDAV.

The project is packaged for PyPI with `maturin`, but day-to-day development does
not require building a wheel. Use Cargo directly while debugging.

## Features

- Interactive account picker for local Codex profiles.
- Account add, switch, refresh, and delete actions inside the interactive UI.
- WebDAV sync from the interactive UI.
- Sanitized `config.toml` sync that excludes machine-local project sections.
- PyPI wheel packaging for users who do not have Rust installed.

## Commands

```shell
authswap
```

`authswap` reads and writes the standard Codex auth layout:

```text
~/.codex/auth.json
~/.codex/config.toml
~/.codex/accounts/*.auth.json
~/.codex/accounts/registry.json
```

## Install From PyPI

```shell
pip install authswap
authswap
```

Published wheels include the compiled `authswap` binary, so end users do not
need Rust, Node.js, or npm when a compatible wheel is available.

## Local Development

Build and run directly with Cargo:

```shell
cargo run --
```

Build the debug binary once, then run it directly:

```shell
cargo build
./target/debug/authswap
```

Install the local Rust binary without building a wheel:

```shell
cargo install --path .
authswap
```

## Isolated Debugging

Use a temporary `HOME` and `CODEX_HOME` while testing so real Codex credentials
are not modified:

```shell
mkdir -p /tmp/authswap-debug/.codex/accounts
HOME=/tmp/authswap-debug \
CODEX_HOME=/tmp/authswap-debug/.codex \
cargo run --
```

## WebDAV Sync

Configure WebDAV sync with `AUTHSWAP_*` environment variables:

```shell
export AUTHSWAP_WEBDAV_URL="https://dav.example.com/authswap/"
export AUTHSWAP_WEBDAV_USERNAME="your-user"
export AUTHSWAP_WEBDAV_PASSWORD="your-password"
```

Bearer token authentication is also supported:

```shell
export AUTHSWAP_WEBDAV_URL="https://dav.example.com/authswap/"
export AUTHSWAP_WEBDAV_TOKEN="your-token"
```

Open WebDAV sync from the account picker by pressing `w`. If WebDAV is not
configured, authswap opens a settings dialog for the URL, username, and
password, then tests the connection before returning to the sync dialog. The
sync dialog shows a green status dot when WebDAV is reachable and a red status
dot otherwise.

The settings file is stored at `~/.codex/authswap.json` with private file
permissions. `AUTHSWAP_*` environment variables override saved WebDAV settings.

These files contain active credentials. Sync only to a trusted WebDAV server.

## Usage Limits

The interactive account picker shows cached 5-hour and weekly limit usage from
`~/.codex/accounts/registry.json`. Press `r` on the selected account to request
fresh usage from Codex and update the local cache.

Limit columns show the remaining percentage and the remaining time until reset, for
example `28% 1h 20m`.

Press `s` in the account picker to configure whether authswap restarts
`codex app-server` after switching accounts. The setting is off by default and
is saved in `~/.codex/authswap.json`.

## Build Wheels

Wheel builds are for packaging and release:

```shell
pip install maturin
maturin build --release
```

The Python package uses `maturin` with `bindings = "bin"` to ship the Rust CLI
as a binary application.

## Publish to PyPI

GitHub Actions builds release artifacts with `maturin` for Linux, macOS, and
Windows, then publishes them to PyPI when a version tag is pushed. The wheel
matrix includes x86/x86_64 and ARM targets:

- Linux: `x86_64`, `i686`, `aarch64`, `armv7`
- macOS: `aarch64`
- Windows: `x86_64`, `i686`, `aarch64`

```shell
git tag v1.0.0
git push origin v1.0.0
```

Before the first release, configure PyPI trusted publishing for this repository:

- PyPI project: `authswap`
- Owner: `shevekwang`
- Repository: `authswap`
- Workflow: `rust-pypi-wheels.yml`
- Environment: `pypi`

Manual workflow runs build and upload artifacts in GitHub Actions, but they do
not publish to PyPI unless the run is for a tag.
