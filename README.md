# Uptime Monitor — wasmCloud Component

A lightweight, always-on network device uptime monitor built as a WebAssembly
component for wasmCloud. It polls your home network devices via HTTP health
checks and serves metrics through a REST API and a live HTML dashboard.

## Architecture

```
┌──────────────────────────────────────────────────┐
│                  wasmCloud Host                  │
│                                                  │
│  ┌─────────────┐    ┌──────────────────┐         │
│  │ HTTP Server ├───►│ uptime-monitor   │         │
│  │  :8080      │    │  (HTTP component)│         │
│  └─────────────┘    └──┬──────────┬────┘         │
│                        │          │    ▲          │
│               ┌────────▼──┐  ┌───▼────┴───────┐  │
│               │HTTP Client│  │   KV Store     │  │
│               └─────┬─────┘  └───▲────────────┘  │
│                     │            │               │
│               ┌─────┴────────────┴────┐          │
│               │  uptime-monitor-service│          │
│               │  (cron service)        │          │
│               │  polls every 1s        │          │
│               │  prunes every 60s      │          │
│               └────────────────────────┘          │
└─────────────────┼────────────────────────────────┘
                  │
          ┌───────▼───────┐
          │ Your Devices  │
          │ (router, NAS, │
          │  Pi, printer) │
          └───────────────┘
```

The project is a Rust workspace with two crates:

- **`component/`** — HTTP component exporting `wasi:http/incoming-handler` (REST API + dashboard) and a `cron` interface
- **`service/`** — Long-running service that imports the `cron` interface, polling all devices every 1 second and pruning old history every 60 seconds

## Prerequisites

- [wash](https://wasmcloud.com/docs/installation) v2.0+
- Rust with `wasm32-wasip2` target: `rustup target add wasm32-wasip2`

## Quick Start

```bash
# 1. Build both components
wash build

# 2. Start wasmCloud + deploy (development mode)
wash dev
```

The API is now live at **http://localhost:8080**.

## Configuration

Edit `.wash/config.yaml` to change:

- **Listen address** — `dev.address` (default `0.0.0.0:8080`)
- **KV backend** — uncomment `wasi_keyvalue_redis_url` for Redis, or use the default in-memory store

## API Reference

### Health Check

```bash
curl http://localhost:8080/health
```

### Register a Device

```bash
curl -X POST http://localhost:8080/api/devices \
  -H "Content-Type: application/json" \
  -d '{
    "id": "living-room-router",
    "name": "Living Room Router",
    "url": "http://192.168.1.1/",
    "expected_status": 200
  }'
```

### List All Devices

```bash
curl http://localhost:8080/api/devices
```

### Delete a Device

```bash
curl -X DELETE http://localhost:8080/api/devices/living-room-router
```

### Get All Metrics

```bash
curl http://localhost:8080/api/metrics
```

### Get Metrics for One Device

```bash
curl http://localhost:8080/api/metrics/living-room-router
```

### Poll All Devices Now

```bash
curl -X POST http://localhost:8080/api/poll
```

### Poll a Single Device

```bash
curl -X POST http://localhost:8080/api/poll/living-room-router
```

### Live Dashboard

Open in your browser:

```
http://localhost:8080/api/dashboard
```

Click any device name to see its detailed status page with 24-hour history.

## Example: Monitoring a Home Network

```bash
# Add your router
curl -X POST localhost:8080/api/devices -H 'Content-Type: application/json' \
  -d '{"id":"router","name":"Main Router","url":"http://192.168.1.1/"}'

# Add a Raspberry Pi
curl -X POST localhost:8080/api/devices -H 'Content-Type: application/json' \
  -d '{"id":"pi","name":"Raspberry Pi","url":"http://192.168.1.50:8080/health"}'

# Add a NAS
curl -X POST localhost:8080/api/devices -H 'Content-Type: application/json' \
  -d '{"id":"nas","name":"Synology NAS","url":"http://192.168.1.100:5000/"}'

# Check the dashboard
open http://localhost:8080/api/dashboard
```

The cron service will automatically start polling all devices every second.

## Publishing

```bash
# Build and push both components to an OCI registry
./update-oci.sh 0.1.0
```

## Deploying to Cosmonic Control

```bash
helm install uptime-monitor \
  -n uptime-monitor --create-namespace \
  -f values.http-trigger.yaml \
  path/to/charts/http-trigger
```

## Metrics Response Shape

```json
{
  "device_id": "router",
  "is_up": true,
  "last_status_code": 200,
  "last_checked": "2026-03-27T14:30:00Z",
  "response_time_ms": 12,
  "total_checks": 288,
  "total_up": 285,
  "total_down": 3,
  "uptime_percentage": 98.9,
  "last_error": null,
  "consecutive_failures": 0
}
```

## License

MIT
