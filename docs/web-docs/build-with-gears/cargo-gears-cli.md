---
title: cargo gears CLI
description: The manifest-driven command-line tool for scaffolding, configuring, building, running, linting, and testing Gears projects.
sidebar:
  label: cargo gears CLI
  order: 2
---

`cargo gears` is the manifest-driven CLI for Gears projects. It scaffolds workspaces, generates runnable servers from a manifest, manages runtime configuration, and wraps build, run, lint, and test.

:::note[Full CLI reference lives with the tool]
The complete, always-current CLI documentation — installation, getting started, the `Gears.toml` manifest, every command, and the LLM/CI workflow — is maintained in the `cargo-gears` repository and published in the **[CLI section](/cli/)** of this site. This page gives the Gears-specific framing; follow the links for the details.
:::

## Install

```sh
cargo install cargo-gears
```

## Typical workflow

```sh
# Scaffold a new workspace
cargo gears new /tmp/cf-demo

# Generate a module and a dev config, then wire the module into the config
cargo gears generate module --template background-worker
cargo gears generate config --template dev --app app1 --env dev
cargo gears config mod add background-worker -c config/app1-dev.yml

# Run the generated server
cargo gears run --app app1 --env dev
```

The generated server is produced from your manifest under `.gears/<app>-<env>/` (the manifest `generated-dir`, default `.gears/`), and runtime configuration is selected with `--app` / `--env` (and `GEARS_CONFIG`).

## Inspect a project

```sh
cargo gears manifest validate       # validate Gears.toml
cargo gears manifest ls --format table  # list manifest entries
cargo gears ls modules              # list active modules
cargo gears src                     # show resolved sources
cargo gears help topic architecture # built-in topic help
cargo gears help schema manifest    # built-in schema help
```

## Where to go next

- **[CLI overview](/cli/)** — the full command-line documentation.
- **[CLI getting started](/cli/getting-started/)** — an end-to-end walkthrough.
- **[Command reference](/cli/commands/)** — every command and flag.
- **[Gears.toml manifest](/cli/manifest/)** — the manifest format.
- **[Install and run](../)** — running the framework's example server without the CLI.
- **[Configure a Gears application](../configure/)** — runtime configuration.
