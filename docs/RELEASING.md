# Releasing MemFork

What to run, in what order, and what cannot be undone.

Everything here needs accounts and tokens that live with a maintainer, so no
step is automated past the point where it becomes public. The pipeline itself
is: push a tag, and `.github/workflows/release.yml` builds six binaries, eight
wheels and an sdist, and publishes a GitHub Release.

## The order

**Nothing irreversible happens before the repository is public.** A crates.io
version can never be reused and a PyPI file can never be replaced, and the work
that precedes going public rewrites history. Publishing first would put a
version built on the old history onto a registry permanently.

### 1. Prove the pipeline with a prerelease

```sh
git tag v0.1.0-rc.1
git push origin v0.1.0-rc.1
```

This builds everything, publishes a GitHub **prerelease**, and publishes to
neither crates.io nor PyPI: the publish job is skipped for prerelease tags and
a job named *publish skipped (prerelease)* runs in its place, so the skip is
visible rather than assumed.

What to check on the run:

- six archives and six `.sha256` files;
- `install.sh` and `install.ps1` attached to the release;
- eight wheels and one sdist;
- every *install (...)* job green — that is D2, `pip install` with no compiler;
- *publish skipped (prerelease)* present, and *publish to PyPI* absent.

Then delete the tag and its release, because the history rewrite below changes
the commit it points at:

```sh
gh release delete v0.1.0-rc.1 --yes
git push origin :refs/tags/v0.1.0-rc.1
git tag -d v0.1.0-rc.1
```

### 2. Make the repository ready to be public

History rewrite, build-process references removed, the files a contributor
expects, wording aimed at a reader rather than at the people who built it.
See DESIGN §9.

### 3. Publish the crates

```sh
cargo publish --workspace --dry-run    # clean before anything is sent
cargo publish -p memfork-core          # the library first
cargo publish -p memfork               # once the index has memfork-core
```

Order matters: the binary depends on the library, and crates.io will refuse
`memfork` until `memfork-core 0.1.0` is there. It usually takes under a minute
to appear; `cargo publish -p memfork` will say if it is not.

`memfork-py` is `publish = false` and never goes to crates.io. The names
`memfork` and `memfork-core` were free as of 2026-09-21.

### 4. Allow the workflow to prove who it is

PyPI trusted publishing works by GitHub handing the workflow a signed token,
which needs the `id-token: write` permission. **This repository cannot issue
one yet**, and the symptom is severe out of proportion to the cause: a run that
requests it is refused before it starts, with no jobs and an error that names
nothing — "This run likely failed because of a workflow file issue".

Turn it on in **Settings → Actions → General → Workflow permissions**, and
check any organisation policy above it. Until then the wheels are built and
tested on every tag and simply not uploaded, which is what a prerelease wanted
anyway.

This is also why `wheels.yml` runs on the tag rather than being called by the
release workflow: a refusal there would have taken the binaries and the GitHub
Release down with it.

### 5. Set up PyPI publishing

Use a **trusted publisher**, so no token is stored anywhere. On PyPI, under the
`memfork` project (or *pending publisher* if the name is not yet claimed):

| Field | Value |
|---|---|
| Owner | `memforkdb` |
| Repository | `memfork` |
| Workflow | `wheels.yml` |
| Environment | `pypi` |

Then create the `pypi` environment in the repository settings. The publish job
already requests `id-token: write` and nothing else.

If a token is used instead, put it in that environment as `PYPI_API_TOKEN` and
give the publish step `with: password: ${{ secrets.PYPI_API_TOKEN }}`. The
trusted publisher is better: there is no secret to leak or rotate.

### 6. Release

```sh
git tag v0.1.0
git push origin v0.1.0
```

This time the publish job runs. Afterwards:

```sh
pip install memfork
curl -fsSL https://github.com/memforkdb/memfork/releases/latest/download/install.sh | sh
```

Both should give version 0.1.0, and `memfork doctor` should agree.

## Changing the release itself

The workflow is generated. Edit `[workspace.metadata.dist]` in `Cargo.toml`,
then:

```sh
dist init --yes        # regenerates .github/workflows/release.yml
```

CI runs `dist generate --check`, so a config change that was not regenerated
fails there rather than surprising somebody mid-release. `dist init` rewrites
the comments in that block, so the reasoning behind the settings lives in
DESIGN §8 as well.

Actions are pinned by commit, in `github-action-commits` and in the other
workflows. To move one, resolve the new commit and change both:

```sh
gh api repos/actions/checkout/commits/v5 --jq .sha
```

## What the version number touches

`version` in the workspace `Cargo.toml`, once. The binary, both libraries and
the wheel all read it from there, and `memfork doctor` prints it. A tag whose
name disagrees with it is the one mistake this pipeline will not catch for
you — check `dist plan` before pushing.
