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

macOS release binaries are signed with a self-signed certificate stored in
1Password (`granola-cli macOS code signing`, Dev Secrets, personal account) and
mirrored to the repo secrets `MACOS_SIGNING_P12` (base64 `.p12`) and
`MACOS_SIGNING_P12_PASSWORD`.

### Why

An ad-hoc signature's designated requirement is the code hash:

```text
designated => cdhash H"f0fb14519cab7d8e1e487ba5e6459652f3bf3d42"
```

That changes with every build, so the macOS keychain treats each release as a
different application, an "always allow" grant never matches again, and users
get a login-password prompt on every upgrade. Signing with a fixed certificate
produces a stable requirement instead:

```text
designated => identifier "com.tmcinerney.granola" and certificate root = H"2af6bd95..."
```

### What this is not

It is not an Apple Developer ID and the binaries are not notarized, so
Gatekeeper rejects them. That does not affect Homebrew installs: `brew`
downloads via curl, which never sets `com.apple.quarantine`, and Gatekeeper only
evaluates quarantined files. Someone downloading a tarball from the Releases
page in a browser *would* see a warning and need `xattr -d com.apple.quarantine`
or right-click → Open.

### Why the certificate is committed

`.github/signing-cert.pem` is the public certificate, checked in deliberately.
The workflow needs it for `security add-trusted-cert`, and extracting it from the
`.p12` is not possible in a way both toolchains accept: a bundle macOS can import
uses legacy RC2-40-CBC, which OpenSSL 3 refuses; one OpenSSL 3 can read uses
AES-256-CBC with a SHA-256 MAC, which makes `security import` fail MAC
verification. A certificate is public, so committing it sidesteps the conflict
and lets anyone verify which identity signs the releases. Only the `.p12` and its
password are secret.

### Rotating or restoring the certificate

GitHub secrets are write-only, so the 1Password item is the only recoverable
copy. If it is lost, future releases sign with a new certificate, the designated
requirement changes, and every existing "always allow" grant breaks — permanently
re-triggering the prompts. To restore:

```sh
op read "op://Dev Secrets/granola-cli macOS code signing/p12_base64" \
  --account my.1password.com | gh secret set MACOS_SIGNING_P12 --repo tmcinerney/granola-cli
op read "op://Dev Secrets/granola-cli macOS code signing/password" \
  --account my.1password.com | gh secret set MACOS_SIGNING_P12_PASSWORD --repo tmcinerney/granola-cli
```

The certificate expires 2036-07-28.
