#!/bin/sh
# Checks a built image the way a deployment would use it. Usage: container-smoke.sh [image]
set -eu
image=${1:-kneetap:local}
containers=""

cleanup() {
    for id in $containers; do docker rm -f "$id" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT INT TERM

# Starts a container and sets `id`. This can't print the ID instead, because a command substitution
# would run in a subshell and lose the update to `containers`.
start() {
    id=$(docker run -d --read-only "$@")
    containers="$containers $id"
}

# Retries a health check while the server starts.
wait_healthy() {
    attempt=0
    until docker exec "$1" /kneetap healthcheck; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 10 ]; then
            docker logs "$1"
            return 1
        fi
        sleep 1
    done
}

# Fails unless the server process runs with this user and group.
expect_identity() {
    actual=$(docker top "$1" -o pid,uid,gid | tail -1 | awk '{print $2":"$3}')
    [ "$actual" = "$2" ] || {
        echo "expected identity $2, found: $actual" >&2
        return 1
    }
}

echo "default start drops root and serves both protocols"
start --health-interval=1s --health-start-period=0s -p 127.0.0.1::80 -p 127.0.0.1::443 "$image"
wait_healthy "$id"
expect_identity "$id" 65532:65532
http=$(docker port "$id" 80/tcp | head -1 | sed 's/.*://')
https=$(docker port "$id" 443/tcp | head -1 | sed 's/.*://')
curl -fsS "http://127.0.0.1:$http/bench" >/dev/null
curl -fsSk "https://127.0.0.1:$https/api" | grep -q '"certificateSource":"generated"'

echo "the image's HEALTHCHECK reports healthy"
attempt=0
until [ "$(docker inspect -f '{{.State.Health.Status}}' "$id")" = healthy ]; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 15 ] || { docker inspect -f '{{json .State.Health}}' "$id"; exit 1; }
    sleep 1
done

echo "an unhealthy status reaches the health check"
curl -fsS -H 'content-type: application/json' -d 503 "http://127.0.0.1:$http/health"
if docker exec "$id" /kneetap healthcheck; then
    echo "healthcheck passed while unhealthy" >&2
    exit 1
fi

echo "stopping is prompt"
started=$(date +%s)
docker stop --time 12 "$id" >/dev/null
[ $(($(date +%s) - started)) -lt 5 ]
[ "$(docker inspect -f '{{.State.ExitCode}}' "$id")" = 0 ]

echo "PUID and PGID choose the identity"
start -e PUID=1000 -e PGID=1001 "$image"
wait_healthy "$id"
expect_identity "$id" 1000:1001

echo "PUID=0 stays root and warns"
start -e PUID=0 "$image"
wait_healthy "$id"
expect_identity "$id" 0:65532
docker logs "$id" 2>&1 | grep -q privileges.root.retained

echo "a non-root user falls back to high ports"
start --user 12345:12345 --cap-drop=ALL "$image"
wait_healthy "$id"

echo "explicit ports reach the health check through flags and the environment"
start "$image" --port 2001 --no-tls
wait_healthy "$id"
start -e KNEETAP_PORT=3000 -e KNEETAP_NO_TLS=true "$image"
wait_healthy "$id"

echo "HTTPS-only servers are probed over HTTPS"
start "$image" --no-http
wait_healthy "$id"

echo "a failed privilege drop stops the server"
start --cap-drop=ALL --cap-add=NET_BIND_SERVICE "$image"
sleep 2
[ "$(docker inspect -f '{{.State.Running}}' "$id")" = false ] || {
    echo "the server kept running without dropping root" >&2
    exit 1
}
[ "$(docker inspect -f '{{.State.ExitCode}}' "$id")" != 0 ]

echo "all container checks passed"
