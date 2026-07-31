# Releasing

This repo ships binaries from GitHub Releases and updates Homebrew separately
through `tmcinerney/homebrew-tap`.

Use this checklist when publishing a new version.

## Prerequisites

- `main` is green in GitHub Actions
- local checkout is clean
- `gh` is authenticated for both `tmcinerney/granola-cli` and
  `tmcinerney/homebrew-tap`

## 1. Cut the release commit

Create a release worktree from the current `main`:

```sh
git fetch origin
git worktree add .worktrees/release-v0.1.4 -b release/v0.1.4 origin/main
cd .worktrees/release-v0.1.4
```

Bump the package version in `Cargo.toml`, then commit it:

```sh
git add Cargo.toml
git commit -m "chore(release): bump version to 0.1.4"
```

`Cargo.lock` does not normally change for a pure version bump. Add it only if
it actually changed.

## 2. Publish the GitHub release

Fast-forward `main`, tag the release commit, and push both:

```sh
git checkout main
git merge --ff-only release/v0.1.4
git tag v0.1.4
git push origin main
git push origin v0.1.4
```

The release workflow in `.github/workflows/release.yml` runs when a `v*` tag
is pushed. It first checks the tag against the version in `Cargo.toml` and fails
immediately if they disagree, so a mismatched tag never produces assets.

The release is created as a **draft**. A final `verify` job checks that all ten
assets are present and only then publishes it. A run where some targets fail
therefore leaves an unpublished draft rather than a release that looks complete —
which is what happened to v0.5.3, published with three of five targets before the
check existed. The two macOS binaries are code-signed with a stable self-signed
certificate before packaging, and the workflow **fails** if the resulting
designated requirement is not certificate-based — `codesign` exits 0 even when
it cannot find the identity, so an unverified step would silently ship unsigned
binaries. See [Code signing](#code-signing) below.

It uploads these assets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `x86_64-pc-windows-msvc`

Watch the workflow until it finishes. It is not done until the `verify` job
has published the draft:

```sh
gh run list --repo tmcinerney/granola-cli --workflow Release --limit 1
gh run watch <run-id> --repo tmcinerney/granola-cli --exit-status
```

## 3. Update the Homebrew tap

Clone the tap somewhere disposable:

```sh
tmpdir=$(mktemp -d /tmp/homebrew-tap-granola-XXXXXX)
gh repo clone tmcinerney/homebrew-tap "$tmpdir"
cd "$tmpdir"
```

Update `Formula/granola-cli.rb`:

- `version`
- macOS arm URL + SHA256
- macOS intel URL + SHA256
- Linux GNU x86_64 URL + SHA256

The formula currently does not use the musl or Windows assets.

The release assets and checksums live on the GitHub Release page. The formula
URLs follow this pattern:

```text
https://github.com/tmcinerney/granola-cli/releases/download/v0.1.4/granola-v0.1.4-aarch64-apple-darwin.tar.gz
https://github.com/tmcinerney/granola-cli/releases/download/v0.1.4/granola-v0.1.4-x86_64-apple-darwin.tar.gz
https://github.com/tmcinerney/granola-cli/releases/download/v0.1.4/granola-v0.1.4-x86_64-unknown-linux-gnu.tar.gz
```

Commit and push the tap update:

```sh
git add Formula/granola-cli.rb
git commit -m "granola-cli 0.1.4"
git push origin main
```

## 4. Validate the install path

On a machine that uses the tap:

```sh
brew update
brew upgrade tmcinerney/tap/granola-cli
granola --version
```

If you already have valid credentials locally, also smoke test auth:

```sh
granola auth status
granola meeting list --since today
```

## 5. Clean up

Remove the release worktree and branch after the release and tap update are
done:

```sh
cd path/to/granola-cli
git worktree remove .worktrees/release-v0.1.4
git branch -d release/v0.1.4
```

## Code signing

macOS release binaries are signed with an **Apple Developer ID Application**
certificate (team `KMWQ959CHU`, personal Apple Developer Program account). The
key is in 1Password as `granola-cli Apple Developer ID signing` (Dev Secrets) and
mirrored to the repo secrets `MACOS_SIGNING_P12` and
`MACOS_SIGNING_P12_PASSWORD`.

### Why

Not for Gatekeeper. Homebrew installs via `curl`, which never sets
`com.apple.quarantine`, and Gatekeeper only evaluates quarantined files — so
these binaries are not notarized and that is fine.

It is for the **keychain**. A "always allow" grant is bound to a partition, and a
signer without an Apple Team Identifier falls back to a per-build `cdhash:`
partition — so every new version re-prompted for the login password on every
keychain read. A Developer ID signature produces a stable `teamid:KMWQ959CHU`
partition and a designated requirement pinned to the team OU:

```text
designated => identifier "com.tmcinerney.granola" and anchor apple generic
  and certificate leaf[subject.OU] = KMWQ959CHU
```

rather than the ad-hoc form, which changes every build:

```text
designated => cdhash H"f0fb14519cab7d8e1e487ba5e6459652f3bf3d42"
```

Verified empirically: four never-granted builds signed this way read the
credential silently, while a self-signed control prompted every time. **A
self-signed certificate cannot substitute** — it has no team identifier, which is
the deciding factor.

### Renewal

The certificate expires **2027-02-01**. Because the requirement pins the team OU
and not the certificate, reissuing under the same team keeps existing keychain
grants working — no user-visible disruption.

### Rotating or restoring the key

Apple allows a Developer ID private key to be downloaded once, so the 1Password
item and the login keychain are the only copies. GitHub secrets are write-only.

```sh
op read "op://Dev Secrets/granola-cli Apple Developer ID signing/p12_base64" \
  --account my.1password.com | gh secret set MACOS_SIGNING_P12 --repo tmcinerney/granola-cli
op read "op://Dev Secrets/granola-cli Apple Developer ID signing/password" \
  --account my.1password.com | gh secret set MACOS_SIGNING_P12_PASSWORD --repo tmcinerney/granola-cli
```

If the key is ever lost entirely, issue a new certificate under the same team —
grants survive, because the requirement pins the OU.
