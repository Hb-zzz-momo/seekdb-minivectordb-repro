#!/usr/bin/env bash
# Smoke test of the evaluation image, run the way the judge runs it
# (STDIO_PROTOCOL.md): JSON requests on stdin, JSON responses on stdout, a
# read-only root file system, no capabilities, no network. Also: data
# surviving a restart and a SIGKILL of the container, and `docker stop`.
#
#   docker build -t minivectordb:smoke .
#   tests/docker_smoke.sh [image]
set -euo pipefail

IMAGE=${1:-minivectordb:smoke}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VOL=mvdb-smoke-$$
NAME=mvdb-smoke-$$
TMP=$(mktemp -d)
JUDGE=(--network=none --cpus=2 --memory=2g --memory-swap=2g --pids-limit=128
       --cap-drop=ALL --security-opt=no-new-privileges --read-only)
cleanup() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker volume rm -f "$VOL" >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# Sends the SQL statements on stdin, one per line, as execute requests
# followed by a shutdown request, in a fresh container on the test volume;
# prints the responses.
run() {
    python3 -c '
import json, sys
n = 0
for n, line in enumerate(sys.stdin.read().splitlines(), 1):
    print(json.dumps({"id": n, "op": "execute", "sql": line, "search_mode": "exact"}, ensure_ascii=False))
print(json.dumps({"id": n + 1, "op": "shutdown"}))' |
        docker run -i --rm "${JUDGE[@]}" -v "$VOL":/data "$IMAGE" 2>"$TMP/stderr"
}

expect_line() {
    grep -qxF -- "$2" <<<"$1" || fail "missing line $2 in:"$'\n'"$1"
}

echo "1. the organizers' checker: handshake, then the public cases"
python3 "$ROOT/protocol_check/run.py" --image "$IMAGE" --handshake-only --output "$TMP/handshake" >"$TMP/check" 2>&1 ||
    fail "handshake: $(cat "$TMP/check")"
python3 "$ROOT/protocol_check/run.py" --image "$IMAGE" --output "$TMP/public" >"$TMP/check" 2>&1 ||
    fail "public cases: $(cat "$TMP/check")"
echo "   $(grep -c ': PASS' "$TMP/check") public cases passed"

echo "2. statements, rollback, filtered TopK, an error"
out=$(run <<'SQL'
CREATE TABLE items (id BIGINT PRIMARY KEY, category VARCHAR(16), embedding VECTOR(3));
INSERT INTO items VALUES (1, 'a', '[0,0,0]'), (2, 'b', '[1,0,0]'), (3, 'a', '[0,2,0]');
SELECT id FROM items WHERE category = 'a' ORDER BY L2_DISTANCE(embedding, '[0,1.9,0]') LIMIT 1;
BEGIN;
DELETE FROM items WHERE id = 1;
ROLLBACK;
SELECT id FROM items ORDER BY id;
SELECT nonsense FROM items;
SQL
)
expect_line "$out" '{"type":"ready"}'
expect_line "$out" '{"id":1,"ok":true}'
expect_line "$out" '{"id":2,"ok":true,"affected_rows":3}'
expect_line "$out" '{"id":3,"ok":true,"columns":["id"],"rows":[[3]]}'
expect_line "$out" '{"id":7,"ok":true,"columns":["id"],"rows":[[1],[2],[3]]}'
grep -q '^{"id":8,"ok":false,"error":{"code":"column_not_found"' <<<"$out" || fail "no column_not_found error in: $out"
expect_line "$out" '{"id":9,"ok":true}'
[ "$(wc -l <<<"$out")" = 10 ] || fail "expected 10 lines on stdout: $out"

echo "3. data survives a restart"
out=$(run <<<'SELECT id FROM items ORDER BY id;')
expect_line "$out" '{"id":1,"ok":true,"columns":["id"],"rows":[[1],[2],[3]]}'

echo "4. acknowledged commit survives SIGKILL of the container"
mkfifo "$TMP/in"
docker run -i --name "$NAME" "${JUDGE[@]}" -v "$VOL":/data "$IMAGE" <"$TMP/in" >"$TMP/out" 2>&1 &
exec 3>"$TMP/in"
echo '{"id":1,"op":"execute","sql":"INSERT INTO items VALUES (4, '"'c'"', '"'[3,3,3]'"');","search_mode":"exact"}' >&3
for _ in $(seq 100); do
    grep -qF '"affected_rows":1' "$TMP/out" 2>/dev/null && break
    sleep 0.1
done
grep -qF '"affected_rows":1' "$TMP/out" || fail "insert was not acknowledged: $(cat "$TMP/out")"
docker kill "$NAME" >/dev/null
exec 3>&-
wait || true
[ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 137 ] || fail "container was not killed by SIGKILL"
docker rm "$NAME" >/dev/null
out=$(run <<<'SELECT id FROM items ORDER BY id;')
expect_line "$out" '{"id":1,"ok":true,"columns":["id"],"rows":[[1],[2],[3],[4]]}'

echo "5. clean exit at end of input without a shutdown request"
docker run -i --rm "${JUDGE[@]}" -v "$VOL":/data "$IMAGE" </dev/null >/dev/null 2>&1 || fail "non-zero exit at end of input"

echo "6. docker stop is a prompt, clean exit"
docker run -i --name "$NAME" "${JUDGE[@]}" -v "$VOL":/data "$IMAGE" <"$TMP/in" >"$TMP/out" 2>&1 &
exec 3>"$TMP/in"
for _ in $(seq 100); do
    grep -qF '{"type":"ready"}' "$TMP/out" 2>/dev/null && break
    sleep 0.1
done
start=$(date +%s)
docker stop "$NAME" >/dev/null
[ $(($(date +%s) - start)) -le 3 ] || fail "docker stop took $(($(date +%s) - start)) s"
exec 3>&-
wait || true
[ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 0 ] || fail "exit code $(docker inspect -f '{{.State.ExitCode}}' "$NAME") after docker stop"
docker rm "$NAME" >/dev/null

echo "PASS ($IMAGE, $(docker image inspect -f '{{.Size}}' "$IMAGE" | awk '{printf "%.1f MB", $1 / 1e6}'))"
