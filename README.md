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
| `/health` | The current health status, which `POST` changes |
| Anything else | The report as text, or JSON if the client prefers it |

To make a replica fail its health checks, for example while testing load balancer failover:

```sh
curl -H 'content-type: application/json' -d 503 localhost:8080/health
```

`kt` is a shorter name for the same program. From another machine,
`kneetap report http://server:8080` prints a report, and `kneetap healthcheck` exits successfully
only if a local server is healthy.

See [CONTRIBUTING](CONTRIBUTING.md) to work on it.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option. Unless you state otherwise, any contribution you submit for inclusion in this project, as
defined in the Apache-2.0 license, is dual licensed as above, without additional terms.
