#!/usr/bin/env bash
# Smoke test of the evaluation image: SQL over stdin, results on stdout,
# committed data surviving a normal restart and a SIGKILL of the container.
#
#   docker build -t minivectordb:smoke .
#   tests/docker_smoke.sh [image]
set -euo pipefail

IMAGE=${1:-minivectordb:smoke}
VOL=mvdb-smoke-$$
NAME=mvdb-smoke-$$
TMP=$(mktemp -d)
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

# Runs SQL from stdin in a fresh container on the test volume; prints stdout.
run() {
    docker run -i --rm -e MVDB_FORMAT=jsonl -v "$VOL":/data "$IMAGE" 2>"$TMP/stderr"
}

expect_line() {
    grep -qxF -- "$2" <<<"$1" || fail "missing line $2 in:"$'\n'"$1"
}

echo "1. statements, rollback, filtered TopK"
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
expect_line "$out" '{"ready":true}'
expect_line "$out" '{"ok":true,"affected":3}'
expect_line "$out" '{"columns":["id"],"rows":[[3]]}'
expect_line "$out" '{"columns":["id"],"rows":[[1],[2],[3]]}'
grep -q '"error":"NoSuchColumn"' <<<"$out" || fail "no NoSuchColumn error in: $out"
[ "$(grep -vc '^{' <<<"$out")" = 0 ] || fail "non-JSON line on stdout: $out"

echo "2. data survives a restart"
out=$(run <<<'SELECT id FROM items ORDER BY id;')
expect_line "$out" '{"columns":["id"],"rows":[[1],[2],[3]]}'

echo "3. acknowledged commit survives SIGKILL of the container"
mkfifo "$TMP/in"
docker run -i --name "$NAME" -e MVDB_FORMAT=jsonl -v "$VOL":/data "$IMAGE" <"$TMP/in" >"$TMP/out" 2>&1 &
exec 3>"$TMP/in"
echo "INSERT INTO items VALUES (4, 'c', '[3,3,3]');" >&3
for _ in $(seq 100); do
    grep -qF '"affected":1' "$TMP/out" 2>/dev/null && break
    sleep 0.1
done
grep -qF '"affected":1' "$TMP/out" || fail "insert was not acknowledged: $(cat "$TMP/out")"
docker kill "$NAME" >/dev/null
exec 3>&-
wait || true
[ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 137 ] || fail "container was not killed by SIGKILL"
docker rm "$NAME" >/dev/null
out=$(run <<<'SELECT id FROM items ORDER BY id;')
expect_line "$out" '{"columns":["id"],"rows":[[1],[2],[3],[4]]}'

echo "4. clean exit on quit"
run <<<'quit' >/dev/null || fail "non-zero exit after quit"
docker run -i --rm -v "$VOL":/data "$IMAGE" </dev/null >/dev/null 2>&1 || fail "non-zero exit at end of input"

echo "PASS ($IMAGE, $(docker image inspect -f '{{.Size}}' "$IMAGE" | awk '{printf "%.1f MB", $1 / 1e6}'))"
