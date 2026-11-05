# Security

Report vulnerabilities through
[GitHub's private reporting](https://github.com/joshka/kneetap/security/advisories/new), not a public
issue.

kneetap shows requests back to whoever sent them, including headers such as cookies and anything a
proxy added on the way. Don't expose it to clients that shouldn't see those details, or behind a
proxy that adds credentials.

Anyone who can reach `/health` can change it with a `POST`, which fails health checks over HTTP and
gRPC, so restrict access where that matters. `--enable-env` shows the server's environment to anyone
who can reach it, so only use it in test environments.
