# Light-Fabric

Light-Fabric is a high-performance, unified platform for managing the lifecycle, governance, and orchestration of enterprise AI services, including agentic services, agents, tools, skills, memories, MCP servers, APIs, gateways, and workflows.

## Overview

Light-Fabric provides the runtime and control-plane foundation for managed agents and distributed AI components. It "weaves" together disparate services into a cohesive, secure, and observable ecosystem, ensuring enterprise-grade governance over autonomous agents and LLM-powered workflows.

## Key Features

- **Unified Control Plane**: A single point of truth for discovering, governing, and auditing agents and APIs via the Light-Portal.
- **Agentic Intelligence**: Built-in support for **Hindsight Memory** (biomimetic memory banks) and centralized agent skills.
- **Enterprise Security**: Fine-grained authorization and data filtering (masking) designed for corporate compliance.
- **High Performance**: Built with Rust, utilizing `tokio` and `axum` for maximum throughput and memory safety.
- **Production Ready**: Out-of-the-box support for retries, failover, and deep observability.

## Documentation

Full documentation, including architecture guides and implementation patterns, is available at:

**[https://networknt.github.io/light-fabric/](https://networknt.github.io/light-fabric/)**

## Core Components

- **`crates/model-provider`**: A unified interface for multiple LLM providers.
- **`frameworks`**: Core infrastructure for high-performance services.
- **`apps`**: Reference applications and enterprise microservices.

## Runtime control-plane capabilities

Light-Runtime answers portal-registry `tools/list` with the versioned
`light-runtime-mcp-capabilities-v1` manifest. The list contains only built-in
tools and tools backed by providers installed in that runtime: logging control,
log-file access, live-log streaming, and cache management are advertised
independently. Invocation checks the provider again and returns a structured
unsupported result when it is absent. Unknown registry requests also return
`unsupported_method`; they no longer receive a generic successful
`{"status":"received"}` acknowledgement.

Live logging accepts an optional bounded `leaseDurationMs` on `start_logs` and
an internal idempotent `renew_logs` call. The V1 controller uses these fields so
a lost `stop_logs` request cannot leave a runtime stream active indefinitely.
Older controllers that omit the lease keep their legacy stream behavior.

## Getting Started

To get started with the Light-Fabric, refer to the [Getting Started](docs/src/getting-started.md) guide in the documentation.

## Release Binaries

GitHub releases should publish the Light-Fabric app binaries for each supported
platform target:

```text
light-agent
light-deployer
light-gateway
light-workflow
```

Install the release targets before building:

```bash
rustup target add x86_64-apple-darwin
rustup target add x86_64-unknown-linux-gnu
rustup target add x86_64-unknown-linux-musl
rustup target add x86_64-pc-windows-msvc
```

Rust target installation provides the Rust standard library for the target. The
platform linker and SDK still need to come from a compatible release runner:
macOS for `x86_64-apple-darwin`, Linux for the GNU and musl Linux targets, and
Windows with the MSVC toolchain for `x86_64-pc-windows-msvc`.

On Debian or Ubuntu Linux runners, install the musl C toolchain before building
`x86_64-unknown-linux-musl`; crates with native C build steps expect
`x86_64-linux-musl-gcc` to be available on `PATH`:

```bash
sudo apt-get update
sudo apt-get install -y musl-tools pkg-config
```

Build, package, and upload the Linux release artifacts with:

```bash
./release.sh v0.1.0
```

To build and package locally without uploading to GitHub:

```bash
./release.sh v0.1.0 --local
```

Build one target from the workspace root:

```bash
TARGET=x86_64-unknown-linux-gnu

cargo build --locked --release --target "${TARGET}" \
  --bin light-agent \
  --bin light-deployer \
  --bin light-gateway \
  --bin light-workflow
```

The binaries are written to `target/${TARGET}/release/`. Windows builds produce
`.exe` files.

Package a Unix-style target:

```bash
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-gnu
ARTIFACT="light-fabric-${VERSION}-${TARGET}"

mkdir -p "dist/${ARTIFACT}"
cp "target/${TARGET}/release/light-agent" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-deployer" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-gateway" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-workflow" "dist/${ARTIFACT}/"
tar -C dist -czf "dist/${ARTIFACT}.tar.gz" "${ARTIFACT}"
```

```bash
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-musl
ARTIFACT="light-fabric-${VERSION}-${TARGET}"

mkdir -p "dist/${ARTIFACT}"
cp "target/${TARGET}/release/light-agent" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-deployer" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-gateway" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-workflow" "dist/${ARTIFACT}/"
tar -C dist -czf "dist/${ARTIFACT}.tar.gz" "${ARTIFACT}"
```

```bash
VERSION=v0.1.0
TARGET=x86_64-apple-darwin
ARTIFACT="light-fabric-${VERSION}-${TARGET}"

mkdir -p "dist/${ARTIFACT}"
cp "target/${TARGET}/release/light-agent" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-deployer" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-gateway" "dist/${ARTIFACT}/"
cp "target/${TARGET}/release/light-workflow" "dist/${ARTIFACT}/"
tar -C dist -czf "dist/${ARTIFACT}.tar.gz" "${ARTIFACT}"
```

Package the Windows target from PowerShell:

```powershell
$Version = "v0.1.0"
$Target = "x86_64-pc-windows-msvc"
$Artifact = "light-fabric-$Version-$Target"

cargo build --locked --release --target $Target `
  --bin light-agent `
  --bin light-deployer `
  --bin light-gateway `
  --bin light-workflow

New-Item -ItemType Directory -Force "dist/$Artifact" | Out-Null
Copy-Item "target/$Target/release/light-agent.exe" "dist/$Artifact/"
Copy-Item "target/$Target/release/light-deployer.exe" "dist/$Artifact/"
Copy-Item "target/$Target/release/light-gateway.exe" "dist/$Artifact/"
Copy-Item "target/$Target/release/light-workflow.exe" "dist/$Artifact/"
Compress-Archive -Force "dist/$Artifact/*" "dist/$Artifact.zip"
```

Create the GitHub release and upload the packaged artifacts:

```bash
VERSION=v0.1.0

gh release create "${VERSION}" \
  dist/light-fabric-"${VERSION}"-x86_64-apple-darwin.tar.gz \
  dist/light-fabric-"${VERSION}"-x86_64-unknown-linux-gnu.tar.gz \
  dist/light-fabric-"${VERSION}"-x86_64-unknown-linux-musl.tar.gz \
  dist/light-fabric-"${VERSION}"-x86_64-pc-windows-msvc.zip \
  --title "${VERSION}" \
  --notes "Light-Fabric release binaries"
```

If the release already exists, upload or replace the artifacts with:

```bash
VERSION=v0.1.0

gh release upload "${VERSION}" \
  dist/light-fabric-"${VERSION}"-x86_64-apple-darwin.tar.gz \
  dist/light-fabric-"${VERSION}"-x86_64-unknown-linux-gnu.tar.gz \
  dist/light-fabric-"${VERSION}"-x86_64-unknown-linux-musl.tar.gz \
  dist/light-fabric-"${VERSION}"-x86_64-pc-windows-msvc.zip \
  --clobber
```

## License

This project is licensed under the Apache-2.0 License.
