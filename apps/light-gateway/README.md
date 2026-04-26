# light-gateway
Light-gateway in rust based on light-pingora

The gateway uses `light-runtime` for config-server bootstrap and controller
registration. Local defaults are under `config/`; config-server values and files
are cached into the runtime external config directory before Pingora starts.

## Docker

Build a local image from the workspace root context:

```bash
./apps/light-gateway/build.sh 0.1.0 --local
```

Run with the local compose file:

```bash
cd apps/light-gateway
docker compose up --build
```
