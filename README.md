# Uptime Monitor — wasmCloud Component

A lightweight, always-on network device uptime monitor built as a WebAssembly
component for wasmCloud. It polls your home network devices via HTTP health
checks and serves metrics through a REST API and a live HTML dashboard.

## Architecture

```
┌─────────────────────────────────────────────┐
│              wasmCloud Host                  │
│                                             │
│  ┌─────────────┐    ┌──────────────────┐    │
│  │ HTTP Server ├───►│ uptime-monitor   │    │
│  │  :8080      │    │  (wasm component)│    │
│  └─────────────┘    └──┬──────────┬────┘    │
│                        │          │         │
│               ┌────────▼──┐  ┌───▼───────┐  │
│               │HTTP Client│  │ Redis KV  │  │
│               └─────┬─────┘  └─────┬─────┘  │
│                     │              │         │
└─────────────────────┼──────────────┼─────────┘
                      │              │
              ┌───────▼───────┐  ┌──▼──┐
              │ Your Devices  │  │Redis│
              │ (router, NAS, │  │     │
              │  Pi, printer) │  └─────┘
              └───────────────┘
```

## Prerequisites

- [wash](https://wasmcloud.com/docs/installation) v0.36+
- Rust with `wasm32-wasip2` target: `rustup target add wasm32-wasip2`
- Redis running locally (for KV persistence): `redis-server`

## Quick Start

```bash
# 1. Build the component
wash build

# 2. Start wasmCloud + deploy (development mode)
wash dev

# 3. Or deploy with the full manifest
wash up -d                          # start wasmCloud host in background
wash app deploy wadm.yaml           # deploy the application
```

The API is now live at **http://localhost:8080**.

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

# Add a printer
curl -X POST localhost:8080/api/devices -H 'Content-Type: application/json' \
  -d '{"id":"printer","name":"Office Printer","url":"http://192.168.1.200/"}'

# Poll everything
curl -X POST localhost:8080/api/poll

# Check the dashboard
open http://localhost:8080/api/dashboard
```

## Automated Polling

The component responds to on-demand poll requests. To set up periodic polling,
use a cron job or a systemd timer on the host machine:

```bash
# crontab -e
# Poll every 5 minutes
*/5 * * * * curl -s -X POST http://localhost:8080/api/poll > /dev/null
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

## Configuration

Edit `wadm.yaml` to change:

- **Listen port** — `source_config.address` on the httpserver link (default `0.0.0.0:8080`)
- **Redis URL** — `target_config.url` on the kvredis link (default `redis://127.0.0.1:6379`)

## License

MIT
