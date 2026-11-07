# kneetap

kneetap is a small server that reports which instance answered a request and what that request
looked like when it arrived. Put it behind a proxy, load balancer, or service mesh to see what
reaches a backend.

It reports over HTTP/1.1, HTTP/2, TLS with client certificates, and gRPC on the same ports, and
echoes WebSocket messages. Connections and requests are bounded, it drops root, and it keeps the
environment private unless you opt in, so it's safe to leave running. It starts even when its ports
or certificates are wrong, and says what it worked around. It ships as a single binary, a `scratch`
image of about 7 MB, or a library for your own tests.

## Try it

```sh
cargo run -p kneetap -- serve --port 8080 --no-tls
curl localhost:8080/anything
```

| Path                           | Response                                                        |
| ------------------------------ | --------------------------------------------------------------- |
| `/api`                         | The report as JSON                                              |
| `/bench`                       | `1`, for load testing                                           |
| `/openapi.yaml`                | A description of this API                                       |
| `/` or `/api` with `?wait=2s`  | The report, after a delay                                       |
| `/` or `/api` with `?env=true` | The report with environment variables, if `--enable-env` is set |
| `/data?size=10&unit=MiB`       | Generated data, with range requests and `?attachment=true`      |
| `/echo`                        | A WebSocket that echoes each message                            |
| `/health`                      | The current health status, which `POST` changes                 |
| Anything else                  | The report as text, or JSON if the client prefers it            |

To make a replica fail its health checks, for example while testing load balancer failover:

```sh
curl -H 'content-type: application/json' -d 503 localhost:8080/health
```

`kt` is a shorter name for the same program. From another machine,
`kneetap report http://server:8080` prints a report, and `kneetap healthcheck` exits successfully
only if a local server is healthy. It tries the default ports at once, so with several servers on a
host, `--name` or `KNEETAP_NAME` picks the one to check. Without a name, every server it finds must
be healthy.

gRPC shares the same port, with the `kneetap.v1.Reporter` service, standard health checks, and
reflection, so `grpcurl -plaintext localhost:8080 kneetap.v1.Reporter/GetReport` works. Paths under
`/kneetap.v1.Reporter/`, `/grpc.health.v1.Health/`, and the reflection services always go to gRPC,
so they never return an HTTP report.

## TLS

kneetap serves HTTP and HTTPS, using a generated self-signed certificate unless you supply one.
Reports show the negotiated session and any client certificate.

```sh
kneetap serve --port 8080 --tls-port 8443
kneetap serve --cert server.pem --key server-key.pem
kneetap serve --no-http --cacert clients.pem
kneetap report https://localhost:8443 --insecure
```

Without `--cacert`, a client certificate is optional and reported as `unverified`, which means
nothing vouches for it. With `--cacert`, HTTPS clients must present a certificate it signed, and a
broken bundle keeps HTTPS off rather than accepting any client.

Certificate files are checked every 30 seconds (`--tls-reload-interval`), and `SIGHUP` rereads them
at once. A replacement is validated before it's used. If it's rejected, the server keeps its
working certificate and reports the failure in `serverStatus.reloadError`.

## Limits

Connections and requests are bounded, so stalled and abandoned connections can't keep capacity for
long. These limits are shared across clients, so a busy client can still use them up. Long-lived
streams hold capacity only while they're in use: a WebSocket counts as a connection until the client
sends no messages for the idle timeout, and a gRPC health watch counts as a request until it has
nothing to report for that long, when it ends as `UNAVAILABLE` so the client can call again. An
HTTP/2 connection closes when a response can't send its next chunk within the blocked write timeout,
even if the client still answers pings. Run `kneetap serve --help` to change any of these.

| Limit                                                              | Default         |
| ------------------------------------------------------------------ | --------------- |
| Connections                                                        | 1024            |
| Requests in progress                                               | 1024            |
| TLS handshake, or first bytes and headers                          | 10 seconds each |
| Reading a request body, or a blocked write                         | 30 seconds each |
| A connection with no requests, or a quiet WebSocket or gRPC stream | 60 seconds      |
| Request body and WebSocket message                                 | 1 MiB each      |
| `/data` response                                                   | 10 GB           |
| `?wait=` delay                                                     | 5 minutes       |

## Startup

kneetap tries to start even when misconfigured, because it's often deployed to find out what's
wrong. Reports list what it worked around under `serverStatus.degradations`: `http_port_moved`
means HTTP is up on its fallback port, and `http_bind_failed` means HTTP is off. HTTPS has the same
pair.

| Problem                            | What happens                                             |
| ---------------------------------- | -------------------------------------------------------- |
| Port 80 or 443 can't be bound      | Moves to 8080 or 8443, unless you set the port yourself  |
| A listener can't start             | The other one serves                                     |
| The certificate or key is unusable | Generates a certificate                                  |
| The `--cacert` bundle is unusable  | HTTPS stays off, and it never accepts any client instead |

`--strict` exits on any of these. By default kneetap listens on all IPv6 and IPv4 addresses. Every
`serve` option except `--puid` and `--pgid` can also be set with a `KNEETAP_` variable, such as
`KNEETAP_PORT`. Flags accept values such as `true`, `false`, `1`, and `0`.

## Container

The image is built from `scratch`, so it's only the binary and works with a read-only filesystem.
Its `HEALTHCHECK` uses `kneetap healthcheck`, which follows the server's settings.

```sh
docker run --read-only -e KNEETAP_NAME=blue -p 8080:80 -p 8443:443 ghcr.io/joshka/kneetap
docker compose up
```

[Docker](docs/docker.md) covers tags, Compose setups, settings, and health checks.

## Privileges

Started as root, kneetap binds its ports, reads its certificates, then switches to UID and GID
65532, or to `--puid` and `--pgid` (`PUID` and `PGID`) if set. It verifies the change and exits if
it didn't work. Files it reloads later must be readable by that user. `PUID=0` stays root, keeps
group 0 unless `PGID` is set, and logs a warning.

## Logs

Logs go to stderr, as text or with `--log-format json`. `--verbose` adds a line per request, and
`KNEETAP_LOG` takes priority over it, using `tracing` filter directives such as
`kneetap_server=debug`. An invalid value is replaced by the default and logged as
`telemetry.filter.rejected`. A server never waits for its logs: if whatever reads stderr falls
behind, new lines are dropped.

## Stopping

On `SIGTERM` or an interrupt, kneetap reports unhealthy, stops accepting connections, closes
WebSockets and quiet gRPC streams, and lets requests in progress finish for up to
`--shutdown-timeout`, 10 seconds by default. `--shutdown-timeout 0s` closes every connection and
exits at once.

## Related projects

- [whoami](https://github.com/traefik/whoami) is the closest project, and inspired this one. kneetap
  covers what it offers and adds more TLS detail, limits, and degraded startup. See
  [Moving from whoami](#moving-from-whoami).
- [go-httpbin](https://github.com/mccutchen/go-httpbin) is a port of httpbin, with a broad set of
  endpoints for testing HTTP clients.
- [http-https-echo](https://github.com/mendhak/docker-http-https-echo) echoes requests as JSON and
  logs them, with options for changing the response.
- [echo-server](https://github.com/jmalloc/echo-server) echoes HTTP, WebSocket, and server-sent
  events for testing proxies.
- [Echo-Server](https://github.com/Ealenn/Echo-Server) lets clients control its responses through
  headers and query parameters.

## Moving from whoami

| With whoami                                        | With kneetap                                      |
| -------------------------------------------------- | ------------------------------------------------- |
| `-port`                                            | `--port`                                          |
| HTTPS                                              | A separate `--tls-port`                           |
| `?env=true`                                        | `?env=true`, once `--enable-env` is set           |
| `/health` updates                                  | `/health` updates as JSON                         |
| `WHOAMI_PORT_NUMBER`, `WHOAMI_NAME`                | `KNEETAP_PORT`, `KNEETAP_NAME`                    |
| gRPC `whoami.Whoami` with `Whoami` and `Bench`     | `kneetap.v1.Reporter` with `GetReport` and `Ping` |
| `/api` fields `ip`, `url`, and an `environ` object | `addresses`, `target`, and an `environment` array |

kneetap also returns `400` for invalid parameters and lowercases header names. For a TLS-only
server, use `--no-http --tls-port`: `--port` alone leaves plain HTTP on, without client
certificates.

## Learn more

- [Docker](docs/docker.md)
- [Architecture](docs/architecture.md)
- [Decisions](docs/adr)
- [Roadmap](docs/roadmap.md)
- [Contributing](CONTRIBUTING.md)
- [Security](SECURITY.md)

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option. Unless you state otherwise, any contribution you submit for inclusion in this project, as
defined in the Apache-2.0 license, is dual licensed as above, without additional terms.
