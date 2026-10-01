# axonal

> [!WARNING]
> **Experimental.** axonal is under active development and not ready for production use.
> Commands, configuration and the cache format may change without notice between releases.

A fast, simple monorepo task runner for pnpm and Cargo workspaces. axonal infers
projects and dependencies from the manifests you already have, runs tasks in
dependency order with a local cache, and works out which projects a change
affects so you only run their tasks. Its command is `ax`.

```sh
ax graph                    # projects, dependencies and targets
ax run build test           # run targets in dependency order, cached
ax run test --affected      # only what changed since the default branch
ax affected                 # list affected projects
ax init                     # write a starter axonal.toml
```

Licensed under the [Mozilla Public License 2.0](LICENSE). Commercial use is
welcome; changes to axonal's own files must be shared back under the same license.
