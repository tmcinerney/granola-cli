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
git worktree add .worktrees/release-v0.1.3 -b release/v0.1.3 origin/main
cd .worktrees/release-v0.1.3
```

Bump the package version in `Cargo.toml`, then commit it:

```sh
git add Cargo.toml
git commit -m "chore(release): bump version to 0.1.3"
```

`Cargo.lock` does not normally change for a pure version bump. Add it only if
it actually changed.

## 2. Publish the GitHub release

Fast-forward `main`, tag the release commit, and push both:

```sh
git checkout main
git merge --ff-only release/v0.1.3
git tag v0.1.3
git push origin main
git push origin v0.1.3
```

The release workflow in `.github/workflows/release.yml` runs when a `v*` tag
is pushed. It uploads these assets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `x86_64-pc-windows-msvc`

Watch the workflow until it finishes:

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
https://github.com/tmcinerney/granola-cli/releases/download/v0.1.3/granola-v0.1.3-aarch64-apple-darwin.tar.gz
https://github.com/tmcinerney/granola-cli/releases/download/v0.1.3/granola-v0.1.3-x86_64-apple-darwin.tar.gz
https://github.com/tmcinerney/granola-cli/releases/download/v0.1.3/granola-v0.1.3-x86_64-unknown-linux-gnu.tar.gz
```

Commit and push the tap update:

```sh
git add Formula/granola-cli.rb
git commit -m "granola-cli 0.1.3"
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
cd /Users/tmcinerney/Code/Public/granola-cli
git worktree remove .worktrees/release-v0.1.3
git branch -d release/v0.1.3
```
