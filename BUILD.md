# Build & Release

## Forges

rspacefs has two CI surfaces:

| Forge | URL | Purpose |
|---|---|---|
| **GitHub** | https://github.com/glennswest/rspacefs | Canonical: issues, PRs, GitHub Releases. Always runs CI on push. |
| **forcicd** | http://forcicd.g8.lo:3000/ci/rspacefs | Local Forgejo mirror on the LAN. Auto-syncs from GitHub every 60 s. Same workflows run here. |

forcicd mirrors GitHub and replays the same workflows. Faster turnaround on the LAN (no public-runner queue, local toolchain images), and **release artifacts get pushed back to GitHub** so the canonical release page on github.com is still the source of truth.

## Workflows

`.github/workflows/ci.yml` — fmt + clippy + tests + cross-build (x86_64 + aarch64 linux). macOS test job gates on `github.server_url == 'https://github.com'` so it doesn't queue forever on forcicd.

`.github/workflows/release.yml` — fires on tag push `v*` or manual `workflow_dispatch`. Builds linux tarballs (x86_64 + aarch64), uploads to GitHub Release. **Always uploads to the GitHub repo** regardless of which forge actually built — on forcicd, uses the `GH_PAT` secret as the GitHub token.

## Release types

Default release type is **alpha**. We stay there until you say otherwise. Two ways to set the type:

1. **Tag suffix**:
   - `v0.1.2-alpha.1` → alpha (prerelease)
   - `v0.1.2-beta.1` → beta (prerelease)
   - `v0.1.2-rc.1` → beta (prerelease)
   - `v0.1.2` → still alpha by default (not promoted automatically)

2. **Manual dispatch** (workflow_dispatch with `release_type` input):
   - `alpha` → prerelease
   - `beta` → prerelease
   - `production` → NOT a prerelease — flips the "Latest release" bit on GitHub

The release name suffix carries the type, e.g. `v0.1.2 (alpha)`, so it's visible on the GitHub Releases page.

## Forcicd secrets

For the forcicd mirror to push releases back to GitHub, the `GH_PAT` secret has to be set on the Forgejo repo:

1. On GitHub, create a fine-grained PAT scoped to `glennswest/rspacefs` only, with `Contents: read & write`.
2. On http://forcicd.g8.lo:3000/ci/rspacefs/settings/actions/secrets, add a secret named `GH_PAT` with that PAT as the value.
3. Done. Next tag push on the forcicd mirror will build and publish to github.com/glennswest/rspacefs/releases.

If `GH_PAT` is missing, the release job will fail loudly — by design. The artifacts are still uploaded to the workflow run, so you can grab them manually.

## Promotion flow

```
push commit → CI green
↓
git tag v0.1.2-alpha.3 && git push origin v0.1.2-alpha.3
↓
forcicd builds linux binaries (~1-2 min) → uploads to GitHub Releases as
"v0.1.2-alpha.3 (alpha)" prerelease
↓
soak / test
↓
git tag v0.1.2-beta.1 (same commit, new tag) && push
↓
forcicd builds again → "v0.1.2-beta.1 (beta)" prerelease
↓
soak / test
↓
git tag v0.1.2 (same commit, new tag) && push    # → still alpha by default
↓
on GitHub Actions UI, fire release.yml manually with release_type=production
   for the v0.1.2 tag → "v0.1.2 (production)", NOT a prerelease, sets the
   "Latest release" pin
```
