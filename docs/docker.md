# kneetap in Docker

kneetap reports which instance answered a request and what that request looked like when it
arrived. The image is built from `scratch` for `linux/amd64`, `linux/arm64`, and `linux/arm/v7`, and
holds only the binary and its licenses, so it runs with a read-only filesystem.

```sh
docker run -d --rm --name kneetap --read-only -p 8080:80 -p 8443:443 ghcr.io/joshka/kneetap
curl --retry 5 --retry-connrefused localhost:8080
docker stop kneetap
```

## Tags

| Tag      | Points at                           |
| -------- | ----------------------------------- |
| `latest` | The newest stable release           |
| `X.Y.Z`  | That release, including prereleases |

Each image carries an SBOM and build provenance.

## Compose

[`compose.yaml`](../compose.yaml) runs one server:

```yaml
services:
  kneetap:
    image: ghcr.io/joshka/kneetap:latest
    read_only: true
    ports:
      - "8080:80"
      - "8443:443"
    environment:
      KNEETAP_NAME: kneetap
    restart: unless-stopped
    stop_grace_period: 15s
```

The entrypoint is `kneetap serve`, so `command:` takes `serve` options, such as
`command: ["--verbose", "--log-format", "json"]`. Every option except `--puid` and `--pgid` can also
be set with a `KNEETAP_` variable. Set the name with `KNEETAP_NAME` rather than `--name`, because
the health check reads the variable to find the server.

### Several replicas behind a proxy

Give each replica a name, and reports show which one answered:

```yaml
services:
  blue:
    image: ghcr.io/joshka/kneetap:latest
    read_only: true
    environment:
      KNEETAP_NAME: blue
  green:
    image: ghcr.io/joshka/kneetap:latest
    read_only: true
    environment:
      KNEETAP_NAME: green
```

Point the proxy at `blue:80` and `green:80`, or `:443` for HTTPS. To make one fail its health
checks, run `curl -H 'content-type: application/json' -d 503 blue/health` from inside the network.

### Your own certificate

Mount the files read-only and name them. kneetap reads them before it drops root, rechecks them every
30 seconds, and rereads them on `SIGHUP` (`docker compose kill -s HUP kneetap`). Files it reloads
must be readable by UID 65532, or by `PUID` and `PGID` if set.

```yaml
services:
  kneetap:
    image: ghcr.io/joshka/kneetap:latest
    read_only: true
    ports:
      - "8443:443"
    volumes:
      - ./certs:/certs:ro
    environment:
      KNEETAP_CERT: /certs/server.pem
      KNEETAP_KEY: /certs/server-key.pem
      KNEETAP_CACERT: /certs/clients.pem
      KNEETAP_NO_HTTP: "true"
```

## Settings

| Variable                        | Effect                                                      |
| ------------------------------- | ----------------------------------------------------------- |
| `KNEETAP_NAME`                  | The name in reports and the one `healthcheck` looks for     |
| `KNEETAP_PORT`                  | The HTTP port inside the container, 80 by default           |
| `KNEETAP_TLS_PORT`              | The HTTPS port inside the container, 443 by default         |
| `KNEETAP_NO_HTTP`               | Turns plain HTTP off                                        |
| `KNEETAP_NO_TLS`                | Turns HTTPS off                                             |
| `KNEETAP_CERT`, `KNEETAP_KEY`   | The server certificate and key, instead of a generated one  |
| `KNEETAP_CACERT`                | Requires client certificates signed by this bundle          |
| `KNEETAP_ENABLE_ENV`            | Allows `?env=true` to show environment variables            |
| `KNEETAP_STRICT`                | Exits instead of working around a bad setting               |
| `KNEETAP_LOG`                   | `tracing` filter directives, such as `kneetap_server=debug` |
| `PUID`, `PGID`                  | The user and group to run as, 65532 by default              |

Logging flags such as `--verbose` and `--log-format json` go in `command:`.

Run `docker run --rm ghcr.io/joshka/kneetap --help` for every option.

## Health checks

The image's `HEALTHCHECK` runs `kneetap healthcheck`, which follows the server's settings. A server
started with `KNEETAP_NO_HTTP` and `KNEETAP_CACERT` only answers clients with a certificate, so the
default check passes once the server asks for one. To check its name and health status too, give the
check a certificate. The image has no shell, so use exec form:

```yaml
healthcheck:
  test: ["CMD", "/kneetap", "healthcheck", "--cert", "/certs/client.pem", "--key", "/certs/client.key"]
```

## Stopping

On `SIGTERM`, kneetap reports unhealthy, stops accepting connections, and lets requests finish for
up to `KNEETAP_SHUTDOWN_TIMEOUT`, 10 seconds by default.
The examples set `stop_grace_period` to 15 seconds, so Compose waits that long before it kills
the server.

See the [README](../README.md) for endpoints, TLS, and limits.
