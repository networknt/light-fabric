# Build Local

This page builds the `light-deployer` binary and container image from the
Light Fabric workspace.

Run all commands from the repository root:

```sh
cd ~/workspace/light-fabric
```

## Rust Build

Use `cargo check` first for a quick compile validation:

```sh
cargo check -p light-deployer
```

Run the deployer tests:

```sh
cargo test -p light-deployer
```

Build a debug binary:

```sh
cargo build -p light-deployer
```

Build a release binary:

```sh
cargo build --release -p light-deployer
```

The release binary is written to:

```text
target/release/light-deployer
```

## Docker Image

Build the local image:

```sh
./apps/light-deployer/build.sh latest
```

The default image name is:

```text
networknt/light-deployer:latest
```

To override the image name:

```sh
IMAGE=localhost:32000/light-deployer:latest ./apps/light-deployer/build.sh latest
```

Verify the image exists:

```sh
docker image inspect networknt/light-deployer:latest
```

## What The Image Contains

The Dockerfile copies:

- `/usr/local/bin/light-deployer`
- `/app/config`

The container runs from `/app`, so the default runtime config directory is:

```text
/app/config
```

The default HTTP port is `7088`, configured in:

```text
apps/light-deployer/config/server.yml
```

## Expected Result

Before moving on, these commands should pass:

```sh
cargo check -p light-deployer
cargo test -p light-deployer
./apps/light-deployer/build.sh latest
docker image inspect networknt/light-deployer:latest
```
