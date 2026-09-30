# axonal

A fast, simple monorepo task runner for pnpm and Cargo workspaces. axonal infers
projects and dependencies from the manifests you already have, runs tasks in
dependency order with a local cache, and knows which tasks a change affects.

```sh
axonal graph                    # projects, dependencies and targets
axonal run build test           # run targets in dependency order, cached
axonal run test --affected      # only what changed since the default branch
axonal affected                 # list affected projects
axonal init                     # write a starter axonal.toml
```

Licensed under the [Mozilla Public License 2.0](LICENSE). Commercial use is
welcome; changes to axonal's own files must be shared back under the same license.
