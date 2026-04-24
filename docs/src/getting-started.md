# Getting Started with Light-Fabric

This guide will help you set up a local development environment for **Light-Fabric**, including the AI Gateway, Agent Engine, and the management Portal.

## Prerequisites

- **Rust**: Latest stable version.
- **Docker**: For running database and backend services.
- **Node.js**: For running the `portal-view` UI.
- **Git**: To clone the necessary repositories.

---

## Local Development Setup

To run the entire ecosystem locally, we use the `portal-config-loc` and `service-asset` repositories to manage configuration and pre-built assets.

### 1. Initialize Workspace

Create a unified workspace directory (e.g., `~/lightapi`) and clone the core management repositories:

```bash
cd ~
mkdir -p lightapi
cd lightapi

# Clone configuration and assets
git clone git@github.com:lightapi/portal-config-loc.git
git clone git@github.com:lightapi/service-asset.git
```

### 2. Deploy Local Services

Light-Fabric services are orchestrated via Docker Compose scripts in `portal-config-loc`. The following command starts the PostgreSQL database and the core services (including the Rust-based components):

```bash
cd ~/lightapi/portal-config-loc
./scripts/deploy-local.sh pg rust
```

### 3. Import Initial Data

Use the importer script in `service-asset` to populate the local database with initial events, users, and configurations:

```bash
cd ~/lightapi/service-asset
./importer.sh -f events.json
```

### 4. Update `/etc/hosts`

The platform uses virtual hosts for local routing. Add the following entry to your `/etc/hosts` file (replace with your actual local IP if necessary):

```text
127.0.0.1  local.lightapi.net locsignin.lightapi.net
```

---

## Running the Management Portal

The **Light-Portal** provides a unified UI for onboarding MCP servers, configuring AI Gateways, and interacting with agents.

```bash
cd ~/lightapi
git clone git@github.com:lightapi/portal-view.git
cd portal-view
npm install
npm run dev
```

Navigate to `https://localhost:3000` and log in with your developer credentials.

---

## Cloud Development (Coming Soon)

We are currently preparing a **Cloud Development Server**. This will allow developers to:
- Connect to a shared, high-performance AI Gateway.
- Onboard and test MCP servers without a full local installation.
- Collaborate on shared agentic workflows and Hindsight memory banks.

Stay tuned for the connection details and onboarding guide for the cloud environment.

---

## Contributing to Light-Fabric

If you are developing for the Rust crates specifically:

```bash
cd ~/lightapi
git clone git@github.com:networknt/light-fabric.git
cd light-fabric
cargo build
```
