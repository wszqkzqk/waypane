# Vendored upstream sources

`waypipe/` is a **read-only git subtree** (one squash commit per upstream release) of
<https://gitlab.freedesktop.org/mstoeckl/waypipe.git>. The upstream tag and its peeled commit
are recorded in `waypipe.upstream.toml`; that file is the single source of truth.

It is excluded from the Cargo workspace. The upstream mainloop requires Linux facilities
such as `memfd`, `pipe2`, and `ppoll`, so it is the protocol reference and interoperability-test
peer, never linked into the clients. The clients run against the stock `waypipe` installed on
the remote host, so patching this copy would only make tests diverge from production.

## Maintenance

Use `scripts/waypipe-upstream.sh` rather than editing `waypipe/`:

- `check`: reports the newest upstream release and whether `waypipe.upstream.toml` needs updating.
- `verify`: compares the vendored tree against an archive of the recorded upstream commit.
  CI runs this on every change; untracked or edited vendor files fail it.
- `update vX.Y.Z`: on a clean tree, imports that release as a subtree squash commit and stages
  the updated manifest. It refuses downgrades. Review and commit the manifest afterwards.
- `export TAG COMMIT DIR`: exports the pinned release for building a second test peer.

Upstream tags are fetched only with `--no-tags` into `refs/waypipe-upstream/*`, and subtree
imports fetch the already-resolved commit from the local repository. Upstream `v*` tags must
never appear in Waypane's `refs/tags`: APK versions come from `git describe`. The script checks
that the tag list does not change during an update.

On upgrade, also compare upstream `protocols/*.xml` with the copies under
`crates/waypane-core/protocols/` before merging. CI covers interoperability against both the
vendored baseline and the newest recorded release; `.github/workflows/upstream-watch.yml`
checks weekly and opens an upgrade PR when a new release appears.
