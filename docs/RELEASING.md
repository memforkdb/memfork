# Releasing MemFork

How to cut a release, in order, and what cannot be undone.

Pushing a version tag runs two workflows. `release.yml` builds six binaries
with checksums, attaches the installers, signs a build provenance attestation
for every artifact, uploads a CycloneDX software bill of materials for each
crate, and publishes a GitHub Release. `wheels.yml` builds eight wheels and an
sdist, installs each one on a runner of its own architecture to prove it needs
no compiler, and uploads them to PyPI, where the publishing action attaches
PEP 740 attestations of its own. The crates go to crates.io separately, by
hand.

## Two things that cannot be undone

**A crates.io version can never be reused.** It can be yanked, which stops new
projects from choosing it, but the number is spent and the files stay
downloadable forever.

**A PyPI file can never be replaced.** Deleting a release does not free its
version, and uploading a corrected file under the same version is refused.

Everything else — a tag, a GitHub Release, a wheel that never left CI — can be
deleted and redone. So if anything about a release is uncertain, rehearse it
with a release candidate first (step 2) and publish only once that is green.

## Before you start

- `main` is green in CI on all three operating systems.
- `CHANGELOG.md` has an entry for the version, describing it for users. The
  `[Unreleased]` section is empty or moved under the new heading.
- **The repository can issue OIDC tokens.** PyPI trusted publishing needs the
  `id-token: write` permission. Check **Settings → Actions → General →
  Workflow permissions**, and any organisation policy above it. See
  [When the release does not start](#when-the-release-does-not-start) for what
  it looks like when this is missing.
- **PyPI knows this repository.** Once, a trusted publisher for the `memfork`
  project (or a *pending publisher* before the first upload):

  | Field | Value |
  |---|---|
  | Owner | `memforkdb` |
  | Repository | `memfork` |
  | Workflow | `wheels.yml` |
  | Environment | `pypi` |

  And a `pypi` environment in the repository settings. No token is stored
  anywhere; the publish job requests `id-token: write` and nothing else.

## 1. Set the version

The version lives in three places in the manifests, and all three must agree:

- `version` under `[workspace.package]` in the root `Cargo.toml`;
- the `memfork-core` entry under `[workspace.dependencies]` in the same file;
- the `memfork` dependency in `crates/memfork-py/Cargo.toml`.

The binary, both libraries, the wheel and `memfork doctor` all read the
version from there. There is deliberately no fourth place: no doc URL, constant
or test carries it. `cargo test` enforces that
(`crates/memfork/tests/version_locations.rs`) and names any line that does, so
if you are tempted to add one, derive it from `CARGO_PKG_VERSION` instead.
`CHANGELOG.md`, `Cargo.lock` and the stores under `tests/fixtures/` are the
only exceptions, since they are history or generated.

Then set the date on the version's heading in
`CHANGELOG.md` to the day the tag will be pushed.

Check that the tag you are about to push matches:

```sh
dist plan --tag vX.Y.Z
```

`dist` derives the release from the package version, so a tag that disagrees
with it produces an error here rather than a broken release later. Commit the
version change on its own.

## 2. Rehearse with a release candidate (optional)

Worth doing whenever the release pipeline, the installers or the wheel matrix
changed since the last release — which is to say, when a failure would be
discovered at the point where it can no longer be fixed quietly.

A candidate needs its own version, because of the check above: set it to
`X.Y.Z-rc.N` on a throwaway branch, tag that commit, and push only the tag.

```sh
git switch -c rc-rehearsal
# set the version to X.Y.Z-rc.N as in step 1, then:
git commit -am "chore: version X.Y.Z-rc.N"
git tag vX.Y.Z-rc.N
git push origin vX.Y.Z-rc.N
git switch main && git branch -D rc-rehearsal
```

SemVer puts a hyphen before the prerelease part and nowhere else, so both
workflows recognise the tag as a prerelease: `release.yml` publishes a GitHub
*prerelease*, and `wheels.yml` builds and tests every wheel but skips PyPI,
running a job named *publish skipped (prerelease)* in its place so the skip is
visible rather than assumed.

What to check on the run:

- six archives, six `.sha256` files, `install.sh` and `install.ps1`;
- eight wheels and one sdist;
- every *install (…)* job green;
- *publish skipped (prerelease)* present and *publish to PyPI* absent.

Then remove it, so that `releases/latest` and anyone browsing tags see only
real releases:

```sh
gh release delete vX.Y.Z-rc.N --yes
git push origin :refs/tags/vX.Y.Z-rc.N
git tag -d vX.Y.Z-rc.N
```

## 3. Publish the crates

```sh
cargo publish --workspace --dry-run    # everything packages and builds
cargo publish -p memfork-core          # the library first
cargo publish -p memfork               # once the index has memfork-core
```

Order matters: the binary depends on the library, and crates.io refuses
`memfork` until the matching `memfork-core` is in the index — usually under a
minute. `memfork-py` is `publish = false`; it reaches users as a wheel.

## 4. Tag

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

This time the publish job in `wheels.yml` runs and uploads to PyPI.

## 5. Verify

On a machine that has never had MemFork installed, or after `memfork stop` and
removing the old one:

```sh
curl -fsSL https://github.com/memforkdb/memfork/releases/latest/download/install.sh | sh
pip install memfork
cargo install memfork
memfork --version
memfork doctor
```

Every one should report the new version. `releases/latest` ignores
prereleases, so the install one-liners only pick up a release once it is a
real one. Check that the README's badges for crates.io, docs.rs and PyPI show
the new version too; docs.rs builds a few minutes after publishing.

Wait for the **Release** workflow, not only **Wheels**, before installing from
`releases/latest`: the installers resolve "latest" to one tag and download
that tag's files, so a half-published release fails cleanly rather than mixing
versions, but a release with no archives yet is still a failed install.

## 6. Check the attestations

Each archive, installer and checksum file on the release carries a build
provenance attestation, signed through GitHub's Sigstore instance by the
workflow that built it (`github-attestations = true` in the dist config).
Anyone with the `gh` command can check a download against it:

```sh
gh attestation verify memfork-x86_64-unknown-linux-musl.tar.xz --repo memforkdb/memfork
```

The output names the workflow, the commit and the tag. To verify on a machine
with no network, download the attestation and the trusted root first, on one
that has:

```sh
gh attestation download memfork-x86_64-unknown-linux-musl.tar.xz --repo memforkdb/memfork
gh attestation trusted-root > trusted_root.jsonl
# then, offline:
gh attestation verify memfork-x86_64-unknown-linux-musl.tar.xz --repo memforkdb/memfork \
  --bundle sha256:<digest>.jsonl --custom-trusted-root trusted_root.jsonl
```

The wheels are attested by PyPI's trusted publishing (PEP 740). To check one:

```sh
pip install pypi-attestations
pypi-attestations verify pypi --repository https://github.com/memforkdb/memfork <wheel URL>
```

Each release also carries a CycloneDX SBOM per crate (`<crate>-vX.Y.Z.cdx.json`),
made by `cargo-cyclonedx` in `sbom.yml`, which runs once the Release workflow
has finished and attaches the files, each with an attestation of its own. It
is a workflow of ours rather than dist's option because dist 0.33.0's
generated step never uploads what it makes (it reads
`steps.cargo-cyclonedx.output.paths`, with `output` for `outputs`). Check
that the SBOM job ran too: it starts a minute or two after Release ends.

GitHub's attestations are free for public repositories. A private fork of
this repository would need GitHub Enterprise Cloud for the same, and `dist`
would say so on the first run.

## When the release does not start

If a tag produces a run with **no jobs at all** and the message *"This run
likely failed because of a workflow file issue"*, the likely cause is a
permission the repository cannot grant — most often `id-token: write`.

GitHub refuses such a run before it starts, and the refusal names nothing. It
is also why `wheels.yml` runs on the tag rather than being called from
`release.yml`: a job that calls a reusable workflow *and* requests
`id-token: write` takes the calling run down with it, so a missing permission
would stop the binaries and the GitHub Release as well as the wheels. As two
workflows, a refusal can only stop the one that asked.

Fix the permission (see [Before you start](#before-you-start)), delete the
tag, and push it again.

## Changing the release pipeline

`release.yml` is generated by `dist` from `[workspace.metadata.dist]` in the
root `Cargo.toml`. Edit the config, then regenerate:

```sh
dist init --yes
```

CI runs `dist generate --check`, so a config change that was not regenerated
fails there rather than surprising somebody mid-release. `dist init` rewrites
the comments in that block, so the reasoning behind each setting also lives in
DESIGN §8.

Every action is pinned to a commit — in `github-action-commits` for the
generated workflow, and directly in the others — because a tag is a name
somebody else can move. To update one, resolve the new commit and change it in
both places:

```sh
gh api repos/actions/checkout/commits/v5 --jq .sha
```
