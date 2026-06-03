# Release With Changelog

Light-Fabric already has a `release.sh` script that builds Linux binaries,
packages release archives, and creates or updates a GitHub release. The current
release page uses a static note string, so operators can download artifacts but
cannot easily see what changed between tags.

This design adds an automated release-notes and changelog flow that keeps four
outputs aligned:

- the generated release notes for one tag
- the GitHub release page body for that tag
- the repository `CHANGELOG.md`
- the released Docker image tags for all Light-Fabric apps

The implementation should start with a small dependency-free git-log script and
leave room to adopt a more structured changelog generator later. It should also
centralize Docker image publishing so binary archives and container images use
the same release version.

## Goals

- Generate release notes from commits between the previous release tag and the
  current release tag.
- Use the same generated notes for GitHub release creation and release updates.
- Maintain a checked-in `CHANGELOG.md` so release history is visible without
  opening GitHub.
- Preserve the current `release.sh VERSION [-l|--local] [--skip-build]`
  operator workflow.
- Release Linux binary archives and Docker images with the same version tag and
  the same compiled Linux binaries.
- Support Apple Silicon and Windows binary artifacts through CI runners that
  match those operating systems.
- Add one repo-root `build.sh` for all Docker images while preserving app-level
  build script compatibility.
- Allow manual edits before publishing when release notes need customer-facing
  cleanup.
- Avoid requiring Conventional Commit messages on day one.

## Non-Goals

- Replace GitHub releases as the artifact distribution point.
- Require every commit message to follow `feat:`, `fix:`, or another convention
  immediately.
- Generate perfect marketing release notes without review.
- Upload changelog files as separate release artifacts.
- Remove existing app-level `build.sh` entrypoints immediately.
- Build macOS binaries from a normal Linux Docker builder. Apple toolchains and
  SDKs require a macOS build runner.
- Build Windows MSVC binaries from a normal Linux Docker builder. Use a Windows
  runner for the official Windows artifacts.
- Publish Windows container images as part of the first release flow. Windows
  container images require Windows base images and a Windows container builder.

## Current State

`release.sh` currently performs these steps:

1. Parse release options and target version.
2. Build `light-agent`, `light-deployer`, `light-gateway`, and
   `light-workflow` for Linux GNU and Linux musl targets.
3. Package the binaries into `dist/light-fabric-${VERSION}-${TARGET}.tar.gz`.
4. If `--local` is not set, create a GitHub release or upload artifacts to an
   existing release.

When creating a new GitHub release, the script uses a static note body:

```text
Light-Fabric Linux release binaries
```

When the release already exists, the script uploads artifacts but does not
update the release notes.

Docker image builds are currently handled by app-level scripts:

```text
apps/light-agent/build.sh
apps/light-deployer/build.sh
apps/light-gateway/build.sh
apps/light-workflow/build.sh
```

Most app scripts use this shape:

```bash
./build.sh 0.3.0
./build.sh 0.3.0 --local
./build.sh 0.3.0 --no-cache
```

Those scripts build and optionally push `networknt/<app>:${VERSION}` and
`networknt/<app>:latest`. `light-deployer` has a simpler custom script, so the
app-level workflow is not completely consistent.

`release.sh` does not currently build or push Docker images. As a result,
binary archives and Docker images can drift if they are released in separate
manual steps or with different version strings.

## Options

### Option 1: GitHub Generated Notes

GitHub CLI can generate release notes:

```bash
gh release create "$VERSION" --generate-notes --notes-start-tag "$PREVIOUS_TAG"
```

This is the least code, and it works well for the GitHub release page. The
tradeoff is that it does not update `CHANGELOG.md` in the repository unless an
additional script calls the GitHub API and copies the generated notes back into
the repo.

This option is useful as a fallback, but it should not be the primary design if
the repo changelog is a required output.

### Option 2: Dependency-Free Git-Log Script

A local script can generate release notes from the git history:

```bash
git log "${PREVIOUS_TAG}..${TARGET_REF}" --pretty=format:"- %s (%h)"
```

The script can write a markdown file and use that same file for both
`CHANGELOG.md` and `gh release create --notes-file`.

This option is simple, reviewable, and fits the current Bash release script. It
does not require new tooling or commit-message conventions. The initial output
will be commit-oriented rather than category-oriented, but it can be improved
incrementally.

### Option 3: `git-cliff`

`git-cliff` can generate structured changelogs from Conventional Commit
messages and custom templates. It can group entries into sections such as
features, fixes, documentation, and breaking changes.

This gives the best long-term release notes, but it adds a release-tool
dependency and works best only after the team consistently writes conventional
commit messages.

This can be adopted later without changing the overall release flow: replace the
internal git-log generator with a `git-cliff` invocation that writes the same
release notes file.

## Proposed Design

Start with Option 2.

Add a helper script:

```text
scripts/release-notes.sh
```

The script should generate:

```text
dist/release-notes-${VERSION}.md
```

It should optionally update:

```text
CHANGELOG.md
```

`release.sh` should call the helper before publishing the GitHub release. The
generated notes file becomes the release page source:

```bash
gh release create "$VERSION" "${ARCHIVES[@]}" \
  --title "$VERSION" \
  --notes-file "$NOTES_FILE"
```

For an existing release, the script should update the release body as well as
uploading artifacts:

```bash
gh release edit "$VERSION" --notes-file "$NOTES_FILE"
gh release upload "$VERSION" "${ARCHIVES[@]}" --clobber
```

Use Docker as the official Linux release builder. The controlled Docker builder
environment should compile Linux binaries once per Linux platform, export those
binaries into `dist/`, and use the same binaries when assembling runtime Docker
images. Local host builds remain useful for development, but they should not be
the official release source for Linux artifacts.

Add a repo-root Docker image script:

```text
build.sh
```

The root script should become the source of truth for building and publishing
all Light-Fabric app images:

```bash
./build.sh 0.3.0
./build.sh 0.3.0 --local
./build.sh 0.3.0 --app light-agent
./build.sh 0.3.0 --image-org networknt --no-cache
```

The script should build these images by default:

```text
networknt/light-agent:0.3.0
networknt/light-deployer:0.3.0
networknt/light-gateway:0.3.0
networknt/light-workflow:0.3.0
```

Unless `--skip-latest` is set, it should also tag and push:

```text
networknt/light-agent:latest
networknt/light-deployer:latest
networknt/light-gateway:latest
networknt/light-workflow:latest
```

Existing app-level build scripts should remain, but they should become thin
wrappers around the root script:

```bash
../../build.sh "$@" --app light-agent
```

This preserves established operator muscle memory and removes duplicated Docker
publish logic.

`release.sh` should call the root `build.sh` with the same `VERSION`. For Linux
targets, the release should build once per platform and reuse the output:

```text
Docker/BuildKit Linux builder
        |
        +-- dist/linux/<target>/bin/<app>       -> GitHub release tarballs
        |
        +-- dist/linux/<target>/bin/<app>       -> Docker runtime images
```

This makes one command release both binary artifacts and Docker images without
compiling the same Linux binaries twice.

## Changelog Format

`CHANGELOG.md` should use reverse chronological release sections:

```markdown
# Changelog

## 0.3.0 - 2026-06-03

- Add JSON file logging support to `light-runtime` (abc1234)
- Wire runtime logging control into `light-gateway` (def5678)
- Document Splunk ingestion options for tracing (123abcd)

## 0.2.0 - 2026-05-20

- ...
```

The generated release notes file should contain the same section body:

```markdown
## 0.3.0 - 2026-06-03

### Changes

- Add JSON file logging support to `light-runtime` (abc1234)
- Wire runtime logging control into `light-gateway` (def5678)
- Document Splunk ingestion options for tracing (123abcd)

### Artifacts

- `light-fabric-0.3.0-x86_64-unknown-linux-gnu.tar.gz`
- `light-fabric-0.3.0-x86_64-unknown-linux-musl.tar.gz`
- `light-fabric-0.3.0-aarch64-unknown-linux-gnu.tar.gz`
- `light-fabric-0.3.0-aarch64-unknown-linux-musl.tar.gz`
- `light-fabric-0.3.0-aarch64-apple-darwin.tar.gz`
- `light-fabric-0.3.0-x86_64-pc-windows-msvc.zip`
- `networknt/light-agent:0.3.0`
- `networknt/light-deployer:0.3.0`
- `networknt/light-gateway:0.3.0`
- `networknt/light-workflow:0.3.0`
```

The release notes file can include artifact names because it is used directly
for the GitHub release page. `CHANGELOG.md` should focus on changes and can
omit artifact details.

Docker images should be listed in the GitHub release body even though they are
published to Docker Hub instead of attached to the release page. This gives
operators one place to see every artifact produced by a release.

Docker image platform variants should also be visible:

```text
networknt/light-agent:0.3.0       linux/amd64, linux/arm64
networknt/light-deployer:0.3.0    linux/amd64, linux/arm64
networknt/light-gateway:0.3.0     linux/amd64, linux/arm64
networknt/light-workflow:0.3.0    linux/amd64, linux/arm64
```

## Tag Range Selection

The release-notes script needs a deterministic commit range.

Inputs:

- `VERSION`: target tag, for example `0.3.0` or `v0.3.0`
- optional `--from PREVIOUS_TAG`
- optional `--target TARGET_REF`

Default behavior:

1. If `--target` is supplied, use it as the end of the range.
2. Else if the `VERSION` tag exists locally, use `VERSION`.
3. Else use `HEAD`.
4. If `--from` is supplied, use it as the start of the range.
5. Else find the newest semver-like tag before `VERSION`.
6. If no previous tag exists, use the first commit as the start.

For existing releases, this allows regenerating the notes for the exact tag. For
new releases, this allows generating notes before the tag exists.

Recommended git command:

```bash
git log --no-merges --pretty=format:"- %s (%h)" "${PREVIOUS_TAG}..${TARGET_REF}"
```

If merge commits are important for the team, the script can add a
`--include-merges` option.

## Release Script Flow

The updated `release.sh` flow should be:

1. Parse release options.
2. Validate build and publish dependencies.
3. Generate release notes into `dist/release-notes-${VERSION}.md`.
4. Build Linux binaries with the Docker release builder unless `--skip-build`
   or `--host-build` is set.
5. Package release archives.
6. Build Docker images unless `--skip-docker` is set.
7. Print generated archive names, Docker image names, and release notes path.
8. If `--local` is set, stop before GitHub and Docker Hub publishing.
9. If the GitHub release exists:
   - update the release body from the generated notes file
   - upload archives with `--clobber`
10. If the GitHub release does not exist:
   - create it with `--notes-file`
   - upload archives during creation
11. Push Docker images unless `--skip-docker` or `--local` is set.

The release notes should be generated before publishing, but the changelog
update should be explicit. A release engineer may want to review and commit
`CHANGELOG.md` before publishing.

Recommended flags:

```text
--update-changelog       prepend the generated section to CHANGELOG.md
--notes-only             generate notes and optionally update changelog without building
--from TAG               override previous tag selection
--target REF             override release notes target ref
--include-merges         include merge commits in generated commit list
--skip-docker            release binary archives only
--docker-only            build and publish Docker images only
--skip-latest            publish VERSION image tags without updating latest
--host-build             use local cargo builds for Linux binaries instead of the Docker release builder
--app APP                restrict Docker image work to one app
--image-org ORG          Docker image namespace, default networknt
--platform PLATFORM      restrict Docker image platform, default linux/amd64,linux/arm64
--skip-macos             skip macOS binary artifacts in CI release mode
--skip-windows           skip Windows binary artifacts in CI release mode
```

`--local` should still build and package locally. It may generate release notes,
but it should not call `gh` or push Docker images.

`--docker-only` should skip binary archive packaging and GitHub release asset
upload. It should still generate release notes by default so the same version
context is visible in the command output. If `--local` is also set, it should
build images locally without pushing them.

## Root Docker Build Script

The repo-root `build.sh` should own Linux Docker image build and push behavior
for all apps.

Recommended app metadata:

| App | Image | Dockerfile |
| --- | --- | --- |
| `light-agent` | `networknt/light-agent` | `apps/light-agent/docker/Dockerfile` |
| `light-deployer` | `networknt/light-deployer` | `apps/light-deployer/Dockerfile` |
| `light-gateway` | `networknt/light-gateway` | `apps/light-gateway/docker/Dockerfile` |
| `light-workflow` | `networknt/light-workflow` | `apps/light-workflow/docker/Dockerfile` |

The Docker build context should remain the workspace root because the
Dockerfiles copy workspace-level `Cargo.toml`, `Cargo.lock`, crates,
frameworks, and app directories.

The script should support:

```text
build.sh [VERSION] [-l|--local] [--no-cache] [--app APP] [--image-org ORG] [--platform PLATFORM] [--skip-latest]
```

Default behavior:

1. Build all app images for `linux/amd64` and `linux/arm64`.
2. Tag each image as `${IMAGE_ORG}/${APP}:${VERSION}`.
3. Tag each image as `${IMAGE_ORG}/${APP}:latest` unless `--skip-latest` is
   set.
4. Use the Linux binaries produced by the release Docker builder instead of
   compiling Rust again inside each runtime image build.
5. If `--local` is set, stop after local image builds.
6. Otherwise push all generated tags and multi-platform manifests.

The script should print the full list of image tags it built and pushed. This
list should be available to `release.sh` so the GitHub release notes can include
the Docker image artifacts.

When `build.sh` is called from `release.sh`, it should receive the exported
binary directory explicitly:

```bash
./build.sh "$VERSION" --binary-dir "dist/build"
```

When `build.sh` is called directly without `--binary-dir`, it can either invoke
the Docker release builder for the requested platforms or fall back to the
current Dockerfile builder stages. The preferred direct behavior is to invoke
the same Docker release builder so local and CI image builds stay aligned.

Recommended implementation:

1. Add a release builder Dockerfile, for example:

```text
docker/Dockerfile.release
```

2. Add a builder target that compiles all apps for one Linux target and exports
   binaries:

```bash
docker buildx build \
  --target export-binaries \
  --platform linux/amd64 \
  --output type=local,dest=dist/build/linux-amd64 \
  .
```

3. Repeat for `linux/arm64` if multi-architecture Linux images are enabled.
4. Package the exported binaries into GitHub release tarballs.
5. Build runtime images from those exported binaries, not from another
   `cargo build`.

The runtime image Dockerfiles can use a binary-only context or a release target
that copies prebuilt binaries:

```dockerfile
COPY dist/build/linux-amd64/bin/light-gateway /app/light-gateway
```

For multi-platform images, `docker buildx build --platform linux/amd64,linux/arm64`
can publish one image tag with a manifest list. The important point is that
each platform-specific image must use the binary built for that platform.

## Cross-Platform Binary Strategy

"Build once" means build once per target platform, then reuse that output
everywhere that platform can run. It does not mean one binary can serve every
operating system and CPU architecture.

Recommended artifact matrix:

| Artifact | Target | Builder |
| --- | --- | --- |
| Linux x86_64 binary archive | `x86_64-unknown-linux-gnu` or `x86_64-unknown-linux-musl` | Docker/BuildKit Linux builder |
| Linux arm64 binary archive | `aarch64-unknown-linux-gnu` or `aarch64-unknown-linux-musl` | Docker/BuildKit Linux builder |
| Linux Docker image for Intel/AMD | `linux/amd64` | Docker/BuildKit Linux builder |
| Linux Docker image for Apple Silicon Docker Desktop | `linux/arm64` | Docker/BuildKit Linux builder |
| Apple Silicon macOS binary archive | `aarch64-apple-darwin` | macOS arm64 runner |
| Windows binary archive | `x86_64-pc-windows-msvc` | Windows runner |

Apple Silicon has two different release meanings:

- Docker image support for Apple Silicon machines is a Linux `arm64` container
  image. Docker Desktop on Apple Silicon runs Linux containers, so
  `linux/arm64` is the right image platform.
- Native Apple Silicon binaries are macOS binaries targeting
  `aarch64-apple-darwin`. These should be built on a macOS runner, not inside a
  normal Linux Docker build.

Windows binaries and Windows container images are also separate concerns:

- Windows binary archives should target `x86_64-pc-windows-msvc` and should be
  built on a Windows runner for the official release.
- Windows container images require Windows base images and a Windows container
  builder. They should be treated as a later phase unless customers explicitly
  need Windows containers.

In CI, these builds can run at the same time as separate jobs:

```text
linux-release:
  Docker/BuildKit builds Linux binaries and Linux Docker images.

macos-release:
  macOS runner builds aarch64-apple-darwin binaries.

windows-release:
  Windows runner builds x86_64-pc-windows-msvc binaries.
```

The release publish job should collect all artifacts and update the same GitHub
release page. Docker Hub publishing should remain in the Linux release job
because the Docker images are Linux container images.

## CHANGELOG Update Strategy

The changelog update should be idempotent.

Rules:

- If `CHANGELOG.md` does not exist, create it with `# Changelog`.
- If a section for `VERSION` already exists, replace that section.
- If no section for `VERSION` exists, insert the new section immediately after
  the `# Changelog` heading.
- Preserve older release sections as-is.
- Never rewrite unrelated content below older release sections.

This makes rerunning the release script safe during release preparation.

## Manual Review Workflow

For a normal release:

```bash
./release.sh 0.3.0 --notes-only --update-changelog
git diff CHANGELOG.md dist/release-notes-0.3.0.md
```

The release engineer reviews and edits `CHANGELOG.md` if needed, commits it,
then publishes:

```bash
./release.sh 0.3.0 --skip-build
```

If binaries also need to be rebuilt:

```bash
./release.sh 0.3.0
```

By default, the official Linux binaries and Linux Docker images should be built
from Docker and published together. If a developer needs the old host-build path
for local troubleshooting:

```bash
./release.sh 0.3.0 --host-build --local
```

If CI is producing all OS artifacts, the release job should collect the
platform-specific archives before publishing:

```text
dist/light-fabric-0.3.0-x86_64-unknown-linux-gnu.tar.gz
dist/light-fabric-0.3.0-aarch64-unknown-linux-gnu.tar.gz
dist/light-fabric-0.3.0-aarch64-apple-darwin.tar.gz
dist/light-fabric-0.3.0-x86_64-pc-windows-msvc.zip
```

If only Docker images need to be rebuilt and pushed with the same release tag:

```bash
./release.sh 0.3.0 --docker-only
```

If only one Docker image needs to be rebuilt locally:

```bash
./build.sh 0.3.0 --app light-gateway --local
```

If the release page already exists and only the notes need refreshing:

```bash
./release.sh 0.3.0 --notes-only
gh release edit 0.3.0 --notes-file dist/release-notes-0.3.0.md
```

The final implementation can make the last command part of `release.sh` when
`--local` is not set.

## GitHub Release Body

The GitHub release body should be generated from the same release notes file.
For new releases:

```bash
gh release create "$VERSION" "${ARCHIVES[@]}" \
  --title "$VERSION" \
  --notes-file "$NOTES_FILE"
```

For existing releases:

```bash
gh release edit "$VERSION" --notes-file "$NOTES_FILE"
gh release upload "$VERSION" "${ARCHIVES[@]}" --clobber
```

This keeps release reruns predictable. Re-uploading artifacts should not leave
stale release notes behind.

## Future Conventional Commit Mode

If the team later adopts Conventional Commits, the helper script can switch from
plain `git log` output to grouped output:

```markdown
### Features

- add JSON tracing output

### Fixes

- preserve ANSI toggle in demo services

### Documentation

- document Splunk ingestion options
```

At that point, `git-cliff` is a good fit. The public contract can remain the
same:

```text
scripts/release-notes.sh VERSION --update-changelog
```

Only the internals of the generator change.

## Risks And Mitigations

| Risk | Mitigation |
| --- | --- |
| Commit messages are too noisy for customer-facing notes | Generate notes early, then review and edit before publishing. |
| Previous tag detection picks the wrong tag | Support `--from TAG` override and print the selected range. |
| Release script rerun duplicates changelog sections | Replace existing `VERSION` section instead of blindly prepending. |
| Existing GitHub release has stale notes after artifact upload | Always call `gh release edit --notes-file` for existing releases. |
| Local builds unexpectedly modify `CHANGELOG.md` | Require explicit `--update-changelog` for file mutation. |
| Binary archives publish but Docker push fails | Build and push images before or immediately after GitHub release publication, print clear recovery commands, and support `--docker-only` reruns. |
| Docker image tags drift from GitHub release version | Have `release.sh` call root `build.sh` with the same `VERSION`; do not ask operators to type the image version separately. |
| Full release builds take longer because Dockerfiles rebuild Rust | Use Docker/BuildKit as the release builder and make runtime images copy exported binaries instead of running another `cargo build`. |
| App-level build scripts diverge again | Convert them to wrappers around repo-root `build.sh`. |
| Apple Silicon image support is confused with macOS binary support | Document that Docker Desktop on Apple Silicon needs `linux/arm64` images, while native macOS binaries need `aarch64-apple-darwin`. |
| Windows artifacts are expected from a Linux Docker build | Build official Windows MSVC binaries on a Windows runner; treat Windows container images as a separate later phase. |

## Implementation Plan

1. Add `CHANGELOG.md` with a short heading and no release entries.
2. Add `scripts/release-notes.sh` with dependency-free git-log generation.
3. Add idempotent changelog insertion or replacement.
4. Add `docker/Dockerfile.release` or equivalent release-builder targets for
   Linux binaries.
5. Add repo-root `build.sh` for all app Docker images and Linux image
   platforms.
6. Convert app-level build scripts into compatibility wrappers.
7. Update `release.sh` to generate `dist/release-notes-${VERSION}.md`.
8. Update `release.sh` to call root `build.sh` with the same `VERSION`, unless
   `--skip-docker` is set.
9. Update runtime image builds to copy binaries exported by the Docker release
   builder instead of compiling Rust again.
10. Add CI matrix jobs for macOS Apple Silicon and Windows binary archives.
11. Update `publish_release()` to use `--notes-file` for both new and existing
   releases.
12. Add README release documentation for the new flags and review workflow.
13. Validate changelog generation locally with:

```bash
./release.sh 0.3.0 --notes-only --update-changelog --local
git diff --check
```

14. Validate Docker image builds locally with:

```bash
./build.sh 0.3.0 --local
./build.sh 0.3.0 --app light-gateway --local
```

15. Validate combined local release packaging with:

```bash
./release.sh 0.3.0 --local
```

16. Validate CI artifact collection for Linux, macOS, and Windows archives.
17. Validate GitHub and Docker Hub publishing on a test tag or draft release
    before using it for a production release.
