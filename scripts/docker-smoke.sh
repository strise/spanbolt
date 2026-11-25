#!/usr/bin/env bash
# Smoke test for the spanbolt image: start it, wait for the Testcontainers ready line, run the REST
# calls a typical e2e setup script makes (create instance and databases, update and get DDL, sessions, SQL), the Python
# client, and the conformance `select` cases against it.
#
#   scripts/docker-smoke.sh                     # image spanbolt:latest
#   IMAGE=spanbolt:arm64 scripts/docker-smoke.sh
#   SMOKE_GRPC=127.0.0.1:9010 SMOKE_REST=127.0.0.1:9020 scripts/docker-smoke.sh   # no Docker
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE="${IMAGE:-spanbolt:latest}"
PROJECT="${SPANNER_PROJECT_ID:-test-project}"
INSTANCE="test-instance"
CONTAINER="spanbolt-smoke-$$"
fail() { echo "FAIL: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

if [ -z "${SMOKE_GRPC:-}" ]; then
  step "start $IMAGE"
  trap 'docker rm -f "$CONTAINER" >/dev/null 2>&1 || true' EXIT
  t0=$(python3 -c 'import time; print(time.time())')
  docker run -d --name "$CONTAINER" -p 127.0.0.1::9010 -p 127.0.0.1::9020 "$IMAGE" >/dev/null
  # The same wait Testcontainers' SpannerEmulatorContainer uses.
  for _ in $(seq 1 300); do
    if docker logs "$CONTAINER" 2>&1 | grep -Eq 'Cloud Spanner emulator running\.'; then break; fi
    sleep 0.02
  done
  docker logs "$CONTAINER" 2>&1 | grep -Eq 'Cloud Spanner emulator running\.' \
    || { docker logs "$CONTAINER"; fail "no ready line"; }
  t1=$(python3 -c 'import time; print(time.time())')
  python3 -c "print(f'container start to ready line: {($t1 - $t0) * 1000:.0f} ms')"
  docker logs "$CONTAINER" 2>&1 | sed 's/^/  log: /'
  step "license texts in /opt/spanbolt/licenses"
  lic=$(mktemp -d)
  docker cp -q "$CONTAINER:/opt/spanbolt/licenses" "$lic/"
  for f in LICENSE NOTICE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt native/README \
           native/{googlesql,abseil-cpp,protobuf,utf8_range,re2,icu,nlohmann_json}/LICENSE; do
    [ -s "$lic/licenses/$f" ] || fail "missing /opt/spanbolt/licenses/$f"
  done
  rm -rf "$lic"
  echo "ok"
  SMOKE_GRPC="127.0.0.1:$(docker port "$CONTAINER" 9010/tcp | head -1 | cut -d: -f2)"
  SMOKE_REST="127.0.0.1:$(docker port "$CONTAINER" 9020/tcp | head -1 | cut -d: -f2)"
fi
REST="http://${SMOKE_REST:?set SMOKE_REST with SMOKE_GRPC}/v1"
echo "gRPC $SMOKE_GRPC, REST $REST"

# Fails unless the HTTP status is 2xx; prints the body.
call() {
  local method="$1" path="$2" body="${3:-}"
  local out code
  out=$(mktemp)
  if [ -n "$body" ]; then
    code=$(curl -s -o "$out" -w '%{http_code}' -X "$method" "$REST/$path" -H 'Content-Type: application/json' --data-raw "$body")
  else
    code=$(curl -s -o "$out" -w '%{http_code}' -X "$method" "$REST/$path")
  fi
  if [ "${code:0:1}" != 2 ]; then
    cat "$out" >&2; rm -f "$out"; fail "$method $path -> HTTP $code"
  fi
  cat "$out"; rm -f "$out"
}
field() { python3 -c "import sys,json; print(json.load(sys.stdin)$1)"; }
# Polls a long-running operation until done, failing on an operation error (spanner-admin.ts).
await_op() {
  local op="$1"
  for _ in $(seq 1 200); do
    local o; o=$(call GET "$op")
    if [ "$(echo "$o" | field ".get('done', False)")" = True ]; then
      echo "$o" | python3 -c "import sys,json; o=json.load(sys.stdin); sys.exit('operation error: %s' % o['error'] if 'error' in o else 0)"
      return
    fi
    sleep 0.025
  done
  fail "operation $op never finished"
}

step "REST: init-spanner.sh"
curl -sf "$REST/projects/$PROJECT/instances" >/dev/null || fail "list instances"
call POST "projects/$PROJECT/instances" "{\"instanceId\": \"$INSTANCE\", \"instance\": {\"config\": \"projects/$PROJECT/instanceConfigs/$PROJECT-emulator-config\", \"displayName\": \"E2E test instance\", \"nodeCount\": 1}}" | field "['name']"
for db in app media; do
  op=$(call POST "projects/$PROJECT/instances/$INSTANCE/databases" "{\"createStatement\": \"CREATE DATABASE \`$db\`\"}" | field "['name']")
  await_op "$op"
  echo "created $db"
done
call GET "projects/$PROJECT/instances" | field "['instances'][0]['name']"
call GET "projects/$PROJECT/instances/$INSTANCE/databases" | python3 -c "import sys,json; print(sorted(d['name'].rsplit('/',1)[1] for d in json.load(sys.stdin)['databases']))"

DB="projects/$PROJECT/instances/$INSTANCE/databases/app"
step "REST: spanner-admin.ts (update DDL, get DDL, drop)"
op=$(call PATCH "$DB/ddl" '{"statements": ["CREATE TABLE Smoke (Id INT64 NOT NULL, Name STRING(MAX), Created TIMESTAMP) PRIMARY KEY (Id)", "CREATE INDEX SmokeByName ON Smoke(Name)"]}' | field "['name']")
await_op "$op"
call GET "$DB/ddl" | python3 -c "import sys,json; s=json.load(sys.stdin)['statements']; assert len(s)==2, s; print(s[1])"
op=$(call POST "projects/$PROJECT/instances/$INSTANCE/databases" '{"createStatement": "CREATE DATABASE `scratch-1`"}' | field "['name']")
await_op "$op"
call DELETE "projects/$PROJECT/instances/$INSTANCE/databases/scratch-1" >/dev/null
echo "dropped scratch-1"

step "REST: sessions, executeSql, transactions, executeBatchDml (seed-data.ts, grant-role.sh)"
SESSION=$(call POST "$DB/sessions" '{}' | field "['name']")
TX=$(call POST "$SESSION:beginTransaction" '{"options": {"readWrite": {}}}' | field "['id']")
call POST "$SESSION:executeSql" "{\"sql\": \"INSERT INTO Smoke (Id, Name, Created) VALUES (1, 'one', CURRENT_TIMESTAMP())\", \"transaction\": {\"id\": \"$TX\"}, \"seqno\": \"1\"}" | field "['stats']"
call POST "$SESSION:commit" "{\"transactionId\": \"$TX\"}" | field "['commitTimestamp']"
TX=$(call POST "$SESSION:beginTransaction" '{"options": {"readWrite": {}}}' | field "['id']")
call POST "$SESSION:executeBatchDml" "{\"transaction\": {\"id\": \"$TX\"}, \"seqno\": \"1\", \"statements\": [{\"sql\": \"INSERT INTO Smoke (Id, Name) VALUES (2, 'two')\"}, {\"sql\": \"UPDATE Smoke SET Name = @n WHERE Id = 1\", \"params\": {\"n\": \"uno\"}, \"paramTypes\": {\"n\": {\"code\": \"STRING\"}}}]}" \
  | python3 -c "import sys,json; r=json.load(sys.stdin); assert r.get('status',{}).get('code',0)==0, r; print([x['stats'] for x in r['resultSets']])"
call POST "$SESSION:commit" "{\"transactionId\": \"$TX\"}" >/dev/null
TX=$(call POST "$SESSION:beginTransaction" '{"options": {"readWrite": {}}}' | field "['id']")
call POST "$SESSION:rollback" "{\"transactionId\": \"$TX\"}" >/dev/null
call POST "$SESSION:executeSql" '{"sql": "SELECT Id, Name FROM Smoke ORDER BY Id"}' \
  | python3 -c "import sys,json; r=json.load(sys.stdin); assert r['rows']==[['1','uno'],['2','two']], r; print(r['metadata']['rowType']['fields'], r['rows'])"
# An error is a non-2xx status with a {code, message, details} body. spanbolt answers a bad query
# with 400 / INVALID_ARGUMENT; the official gateway fails to marshal it and answers 500.
resp=$(curl -s -w '\n%{http_code}' -X POST "$REST/$SESSION:executeSql" -H 'Content-Type: application/json' -d '{"sql": "SELECT * FROM Nope"}')
code=$(echo "$resp" | tail -1)
[ "${code:0:1}" != 2 ] || fail "bad query gave HTTP $code"
echo "$resp" | head -1 | python3 -c "import sys,json; e=json.load(sys.stdin); assert isinstance(e['code'], int), e; print('bad query -> HTTP $code', e)"
resp=$(curl -s -w '\n%{http_code}' "$REST/projects/$PROJECT/instances/missing")
echo "$resp" | tail -1 | grep -q 404 || fail "missing instance: $resp"
echo "$resp" | head -1 | python3 -c "import sys,json; e=json.load(sys.stdin); assert e['code']==5 and e['details'][0]['@type'].endswith('ResourceInfo'), e; print('missing instance -> HTTP 404 with ResourceInfo')"

step "Python client (google-cloud-spanner)"
SPANNER_EMULATOR_HOST="$SMOKE_GRPC" python3 - "$PROJECT" "$INSTANCE" <<'EOF'
import sys
from google.cloud import spanner
project, instance_id = sys.argv[1], sys.argv[2]
client = spanner.Client(project=project)
instance = client.instance(instance_id)
db = instance.database("py-smoke", ddl_statements=[
    "CREATE TABLE Singers (SingerId INT64 NOT NULL, Name STRING(MAX)) PRIMARY KEY (SingerId)"])
db.create().result(30)
with db.batch() as b:
    b.insert("Singers", ("SingerId", "Name"), [(i, f"s{i}") for i in range(100)])
def txn(t):
    return t.execute_update("UPDATE Singers SET Name = 'x' WHERE SingerId < 10")
assert db.run_in_transaction(txn) == 10
with db.snapshot() as s:
    rows = list(s.execute_sql("SELECT COUNT(*), COUNTIF(Name = 'x') FROM Singers"))
assert rows == [[100, 10]], rows
with db.snapshot() as s:
    got = list(s.read("Singers", ("SingerId", "Name"), spanner.KeySet(keys=[[5]])))
assert got == [[5, "x"]], got
print("python client ok:", rows)
db.drop()
EOF

step "conformance: --tag select (default mode, so Cloud Spanner expectations)"
cargo run -q --release -p spanbolt-conformance -- check --target "$SMOKE_GRPC" --tag select --oracle spanner

printf '\nSMOKE OK\n'
