# kneetap

kneetap is a small server that reports which instance answered a request and what that request
looked like when it arrived.

It's under active development. To try it:

```sh
cargo run -p kneetap -- serve --port 8080
curl localhost:8080/anything
```

| Path | Response |
| --- | --- |
| `/api` | The report as JSON |
| `/bench` | `1`, for load testing |
| `/` or `/api` with `?wait=2s` | The report, after a delay |
| `/` or `/api` with `?env=true` | The report with environment variables, if `--enable-env` is set |
| `/data?size=10&unit=MiB` | Generated data, with range requests and `?attachment=true` |
| `/echo` | A WebSocket that echoes each message |
| `/health` | The current health status, which `POST` changes |
| Anything else | The report as text, or JSON if the client prefers it |

To make a replica fail its health checks, for example while testing load balancer failover:

```sh
curl -H 'content-type: application/json' -d 503 localhost:8080/health
```

`kt` is a shorter name for the same program. From another machine,
`kneetap report http://server:8080` prints a report, and `kneetap healthcheck` exits successfully
only if a local server is healthy.

gRPC shares the same port, with the `kneetap.v1.Reporter` service, standard health checks, and
reflection, so `grpcurl -plaintext localhost:8080 kneetap.v1.Reporter/GetReport` works.

## TLS

`--tls-port` also serves HTTPS, with a generated self-signed certificate unless you supply one.
Reports show the negotiated session and any client certificate.

```sh
kneetap serve --tls-port 8443
kneetap serve --tls-port 8443 --cert server.pem --key server-key.pem
kneetap serve --tls-port 8443 --no-http --cacert clients.pem
kneetap report https://localhost:8443 --insecure
```

Without `--cacert`, a client certificate is optional and reported as `unverified`, which means
nothing vouches for it. With `--cacert`, HTTPS clients must present a certificate it signed, and a
broken bundle stops the server rather than accepting any client.

Certificate files are checked every 30 seconds (`--tls-reload-interval`), and `SIGHUP` rereads them
at once. A replacement is validated before it's used. If it's rejected, the server keeps its
working certificate and reports the failure in `serverStatus.reloadError`.

## Limits

Connections and requests are bounded, so stalled and abandoned connections can't keep capacity for
long. These limits are shared across clients, so a busy client can still use them up. Long-lived
streams hold capacity while they stay open: a WebSocket counts as a connection, and a gRPC health
watch counts as a request. Run `kneetap serve --help` to change any of these.

| Limit | Default |
| --- | --- |
| Connections | 1024 |
| Requests in progress | 1024 |
| TLS handshake, or first bytes and headers | 10 seconds each |
| Reading a request body, or a blocked write | 30 seconds each |
| A connection with no requests | 60 seconds |
| Request body and WebSocket message | 1 MiB each |
| `/data` response | 10 GB |
| `?wait=` delay | 5 minutes |

## Stopping

On `SIGTERM` or an interrupt, kneetap reports unhealthy, stops accepting connections, closes
WebSockets, and lets requests in progress finish for up to `--shutdown-timeout`, 10 seconds by
default.

See [CONTRIBUTING](CONTRIBUTING.md) to work on it.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option. Unless you state otherwise, any contribution you submit for inclusion in this project, as
defined in the Apache-2.0 license, is dual licensed as above, without additional terms.
