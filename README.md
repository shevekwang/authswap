# authswap

`authswap` is a Rust command-line tool for switching local Codex auth profiles
and optionally syncing those profiles through WebDAV.

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

## WebDAV Sync

authswap supports custom WebDAV services. Press `w` in the account picker to
configure the server URL and credentials, test the connection, and push or pull
account data. When no local accounts exist, the first-run dialog also offers
WebDAV so accounts can be restored without signing in first.

The settings file is stored at `~/.codex/authswap.json` with private file
permissions.

These files contain active credentials. Sync only to a trusted WebDAV server.

## Usage Limits

The interactive account picker shows cached 5-hour and weekly limit usage from
`~/.codex/accounts/registry.json`. Press `r` on the selected account to request
fresh usage from Codex. Press `t` to refresh all accounts, with 100ms between
requests. Both actions update the local cache.

Limit columns show the remaining percentage and the remaining time until reset, for
example `28% 1h 20m`.

Press `s` in the account picker to configure whether authswap restarts
`codex app-server` after switching accounts. The setting is off by default and
is saved in `~/.codex/authswap.json`.
