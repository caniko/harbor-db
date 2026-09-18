#!/usr/bin/env bash
# Disposable Gel integration test for harbor-db (host-side, podman-based).
#
# Spins up real Gel 7.1 servers (pinned image), drives them through a toy
# project-owned migration fixture via the generic harbor-db runner, and
# asserts the full behavior matrix. Cleans up containers and state on exit.
#
# Usage: ./live.sh [--stacks 2] [--image docker.io/geldata/gel:7.1]
# Needs: podman, jq, sha256sum, repo-built ./target/debug/harbor-db.
set -eu
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
STACKS=2
IMAGE="docker.io/geldata/gel:7.1"
HARBOR_BIN="${HARBOR_BIN:-$REPO/target/debug/harbor-db}"
while [ $# -gt 0 ]; do case "$1" in
  --stacks) STACKS="$2"; shift 2;;
  --image) IMAGE="$2"; shift 2;;
  *) echo "usage: $0 [--stacks N] [--image IMG]" >&2; exit 64;;
esac; done

pass=0; fail=0
ok() { pass=$((pass+1)); echo "ok $pass - $1"; }
bad() { fail=$((fail+1)); echo "not ok - $1"; }

# One full stack: disposable server + fixture + harbor-db assertions.
run_stack() {
  local id="$1" port="$2"
  local ctr="harbor-db-gel-test-$id" T marks0 marks1
  T="$(mktemp -d "/tmp/harbor-db-gel-$id-XXXXXX")"
  mkdir -p "$T"/{data,share,state,migrations,credsdir,bin}
  cp "$REPO/tests/gel/migrations/"*.edgeql "$T/migrations/"
  local admin_pw="admin-pw-$id-$(date +%s)"
  printf '%s' "$admin_pw" >"$T/pw"
  printf '{"host":"127.0.0.1","port":5656,"user":"admin","password":"%s","branch":"main","tls_security":"insecure"}' "$admin_pw" >"$T/share/creds.json"
  cp "$T/share/creds.json" "$T/credsdir/gel-creds"
  cat >"$T/bin/gel" <<EOF
#!/usr/bin/env bash
# Fixture-only path translation: the toy runs on the host, gel runs inside
# the container. Production units run host-natively and need no translation.
args=()
for a in "\$@"; do
  case "\$a" in
    "$T/credsdir/"*) a="/creds/\${a#$T/credsdir/}" ;;
    "$T/migrations/"*) a="/migrations/\${a#$T/migrations/}" ;;
  esac
  args+=("\$a")
done
exec podman exec "$ctr" gel "\${args[@]}"
EOF
  chmod +x "$T/bin/gel"
  export PATH="$T/bin:$PATH"

  cleanup_stack() {
    podman stop -t 10 "$ctr" >/dev/null 2>&1 || true
  }

  podman run -d --rm --userns="keep-id:uid=$GEL_UID,gid=$GEL_GID" --name "$ctr" -p "127.0.0.1:$port:5656" \
    -v "$T/data:/var/lib/gel/data" -v "$T/share:/share:ro" \
    -v "$T/credsdir:/creds:ro" -v "$T/migrations:/migrations:ro" \
    -v "$T/pw:/run/secrets/pw:ro" \
    -e GEL_SERVER_PASSWORD_FILE=/run/secrets/pw \
    -e GEL_SERVER_TLS_CERT_MODE=generate_self_signed \
    "$IMAGE" server >/dev/null
  local i=0
  until gel --credentials-file /share/creds.json --connect-timeout 3s query "select 1" >/dev/null 2>&1; do
    i=$((i+1)); [ "$i" -lt 45 ] || { bad "stack $id: server never became ready"; cleanup_stack; return 1; }
    sleep 2
  done
  ok "stack $id: server startup and authenticated readiness"
  [ "$(gel --credentials-file /share/creds.json query "select 1")" = "1" ] \
    && ok "stack $id: authenticated EdgeQL query returns data" \
    || { bad "stack $id: query wrong result"; cleanup_stack; return 1; }
  gel --version >"$T/versions" 2>&1
  gel --credentials-file /share/creds.json query "select sys::get_version_as_str()" >>"$T/versions" 2>&1
  # Same flag sequence the Nix readyCheck script uses (password via stdin file).
  podman exec -i "$ctr" gel --host 127.0.0.1 --port 5656 --user admin \
    --password-from-stdin --tls-security insecure --connect-timeout 5s \
    --wait-until-available 30s query "select 1" <"$T/pw" >/dev/null 2>&1 \
    && ok "stack $id: password-from-stdin readiness (readyCheck mechanism)" \
    || { bad "stack $id: password-from-stdin readiness"; cleanup_stack; return 1; }

  # harbor-db plan: schema (toy migrate) -> app (dependent) + operator wipe.
  # Credential references are names only; harbor-db resolves them under
  # $CREDENTIALS_DIRECTORY at runtime and never serializes secret contents.
  TOY="$REPO/tests/gel/toy-chaosbox" STATE="$T/state" MIG="$T/migrations" MARK="$T/app-marker" EV="$T/app-events" WIPED="$T/state/wiped" jq -n '{
    version: 1, name: "gel-toy",
    operations: [
      {id: "schema", kind: "database", lifecycle: "ensure", backend: "gel", phase: "schema", safety: "automatic",
       apply: {program: env.TOY, args: ["db","migrate","--json"],
         environment: {TOY_STATE_DIR: env.STATE, TOY_MIGRATIONS_DIR: env.MIG},
         credential_environment: {CHAOSBOX_GEL_CREDENTIALS_FILE: "gel-creds"}},
       check: {program: env.TOY, args: ["db","check","--json"],
         environment: {TOY_STATE_DIR: env.STATE, TOY_MIGRATIONS_DIR: env.MIG},
         credential_environment: {CHAOSBOX_GEL_CREDENTIALS_FILE: "gel-creds"}},
       depends_on: []},
      {id: "app", kind: "generic", lifecycle: "ensure", backend: "generic", phase: "schema", safety: "automatic",
       apply: {program: "sh", args: ["-c", "touch \(env.MARK); echo app >> \(env.EV)"]},
       check: {program: "test", args: ["-f", env.MARK]}, depends_on: ["schema"]},
      {id: "wipe", kind: "database", lifecycle: "ensure", backend: "gel", phase: "operational", safety: "operator_confirmed",
       apply: {program: "sh", args: ["-c", "touch \(env.WIPED)"]},
       check: {program: "test", args: ["-f", env.WIPED]}, depends_on: ["schema"]}
    ]}' >"$T/plan.json"

  export CREDENTIALS_DIRECTORY="$T/credsdir"
  export TOY_STATE_DIR="$T/state" TOY_MIGRATIONS_DIR="$T/migrations"

  code=0; "$HARBOR_BIN" check --manifest "$T/plan.json" --operation schema >"$T/check1.log" 2>&1 || code=$?
  { [ "$code" -eq 2 ] && grep -q pending "$T/check1.log"; } \
    && ok "stack $id: read-only check reports pending (exit 2), state untouched" \
    || { bad "stack $id: pending check (code=$code)"; cleanup_stack; return 1; }
  [ ! -e "$T/state/applied" ] || [ ! -s "$T/state/applied" ] \
    && ok "stack $id: check applied nothing" \
    || { bad "stack $id: check was not read-only"; cleanup_stack; return 1; }

  "$HARBOR_BIN" apply --manifest "$T/plan.json" >"$T/apply1.log" 2>&1 \
    && ok "stack $id: apply migrates and starts dependent" \
    || { bad "stack $id: apply failed"; cleanup_stack; return 1; }
  grep -q "wipe: skipped-manual" "$T/apply1.log" && [ ! -e "$T/state/wiped" ] \
    && ok "stack $id: operator-confirmed wipe excluded from activation" \
    || { bad "stack $id: wipe gating"; cleanup_stack; return 1; }
  [ "$(wc -l <"$T/state/applied")" -eq 3 ] && [ -f "$T/app-marker" ] \
    && ok "stack $id: 3 committed migrations applied once, dependent ran" \
    || { bad "stack $id: applied state wrong"; cleanup_stack; return 1; }
  [ "$(gel --credentials-file /share/creds.json query --output-format json "select count((select ToyItem))")" = "[1]" ] \
    && ok "stack $id: migrated data visible in Gel" \
    || { bad "stack $id: seed row missing"; cleanup_stack; return 1; }

  "$HARBOR_BIN" apply --manifest "$T/plan.json" >/dev/null 2>&1 \
    && [ "$(wc -l <"$T/state/applied")" -eq 3 ] \
    && ok "stack $id: repeated apply is idempotent" \
    || { bad "stack $id: idempotency"; cleanup_stack; return 1; }
  "$HARBOR_BIN" check --manifest "$T/plan.json" --operation schema --operation app >/dev/null 2>&1 \
    && ok "stack $id: post-migration check is current (exit 0)" \
    || { bad "stack $id: current check"; cleanup_stack; return 1; }

  # Credential separation: reader can read, cannot migrate.
  printf '{"host":"127.0.0.1","port":5656,"user":"toy_reader","password":"toy-reader-pw","branch":"main","tls_security":"insecure"}' >"$T/credsdir/gel-reader-creds"
  TOY="$REPO/tests/gel/toy-chaosbox" STATE="$T/state" MIG="$T/migrations" jq '
    .operations[0].apply.credential_environment = {CHAOSBOX_GEL_CREDENTIALS_FILE: "gel-reader-creds"}
    | .operations[0].check.credential_environment = {CHAOSBOX_GEL_CREDENTIALS_FILE: "gel-reader-creds"}' \
    "$T/plan.json" >"$T/plan-reader.json"
  "$HARBOR_BIN" check --manifest "$T/plan-reader.json" --operation schema >/dev/null 2>&1 \
    && ok "stack $id: reader credentials pass read-only check" \
    || { bad "stack $id: reader check"; cleanup_stack; return 1; }
  printf '\ncreate type ToyShouldNotExist { create required property name -> str; };\n' >"$T/migrations/0004-reader-attempt.edgeql"
  code=0; "$HARBOR_BIN" apply --manifest "$T/plan-reader.json" --operation schema >"$T/reader.log" 2>&1 || code=$?
  rm "$T/migrations/0004-reader-attempt.edgeql"
  { [ "$code" -ne 0 ] && grep -q "permission" "$T/reader.log"; } \
    && ok "stack $id: reader credentials cannot migrate (permission denied)" \
    || { bad "stack $id: reader migrate not rejected (code=$code)"; cleanup_stack; return 1; }

  # Incompatible migration blocks dependents without applying.
  marks0="$(wc -l <"$T/app-events")"
  printf -- '-- REQUIRES-MAJOR: 99\ncreate type ToyFuture { create required property name -> str; };\n' >"$T/migrations/0004-future.edgeql"
  code=0; "$HARBOR_BIN" check --manifest "$T/plan.json" --operation schema >"$T/incompat.log" 2>&1 || code=$?
  # NOTE: the toy exits 3 (incompatible) but harbor-db surfaces any non-pending
  # check failure as exit 1; the incompatible JSON status stays in the log.
  { [ "$code" -eq 1 ] && grep -q incompatible "$T/incompat.log"; } \
    && ok "stack $id: incompatible status surfaces (blocked, distinct from pending)" \
    || { bad "stack $id: incompatible check (code=$code)"; cleanup_stack; return 1; }
  code=0; "$HARBOR_BIN" apply --manifest "$T/plan.json" >/dev/null 2>&1 || code=$?
  marks1="$(wc -l <"$T/app-events")"
  { [ "$code" -ne 0 ] && [ "$marks0" = "$marks1" ]; } \
    && ok "stack $id: incompatible migration blocks dependent startup" \
    || { bad "stack $id: incompatible did not block (code=$code)"; cleanup_stack; return 1; }
  rm "$T/migrations/0004-future.edgeql"

  # Broken EdgeQL fails the migration and blocks the dependent.
  printf 'create !!! this is not valid edgeql !!!\n' >"$T/migrations/0004-broken.edgeql"
  code=0; "$HARBOR_BIN" apply --manifest "$T/plan.json" >"$T/broken.log" 2>&1 || code=$?
  marks1="$(wc -l <"$T/app-events")"
  { [ "$code" -ne 0 ] && [ "$marks0" = "$marks1" ] && grep -q failed "$T/broken.log"; } \
    && ok "stack $id: failed migration blocks dependent startup" \
    || { bad "stack $id: broken migration did not block (code=$code)"; cleanup_stack; return 1; }
  rm "$T/migrations/0004-broken.edgeql"
  "$HARBOR_BIN" apply --manifest "$T/plan.json" >/dev/null 2>&1 \
    && ok "stack $id: apply recovers after bad migration is removed" \
    || { bad "stack $id: recovery"; cleanup_stack; return 1; }

  # Wrong password is an authenticated error, not pending/ready.
  printf '%s' '{"host":"127.0.0.1","port":5656,"user":"admin","password":"wrong-pw","branch":"main","tls_security":"insecure"}' >"$T/credsdir/gel-creds-bad"
  TOY="$REPO/tests/gel/toy-chaosbox" STATE="$T/state" MIG="$T/migrations" jq '
    .operations[0].check.credential_environment = {CHAOSBOX_GEL_CREDENTIALS_FILE: "gel-creds-bad"}' \
    "$T/plan.json" >"$T/plan-bad.json"
  code=0; "$HARBOR_BIN" check --manifest "$T/plan-bad.json" --operation schema >"$T/bad.log" 2>&1 || code=$?
  { [ "$code" -eq 1 ] && grep -q '"status":"error"' "$T/bad.log"; } \
    && ok "stack $id: wrong password is an error (exit 1), not ready/pending" \
    || { bad "stack $id: bad-password semantics (code=$code)"; cleanup_stack; return 1; }

  # Hygiene: the admin password lives only in credential files, never in plans/logs.
  if grep -rq "$admin_pw" "$T/plan.json" "$T/plan-reader.json" "$T/plan-bad.json" "$T"/*.log "/tmp/harbor-db-gel-stack-s${id#s}.log"; then
    bad "stack $id: secret leaked into plan or logs"
    cleanup_stack; return 1
  fi
  ok "stack $id: credential files stay outside plans and logs"

  # Explicit confirmation runs the operator operation.
  "$HARBOR_BIN" apply --manifest "$T/plan.json" --operation wipe --confirm >/dev/null 2>&1 \
    && [ -e "$T/state/wiped" ] \
    && ok "stack $id: explicit --operation wipe --confirm runs" \
    || { bad "stack $id: confirmed wipe"; cleanup_stack; return 1; }

  echo "--- stack $id versions ---"; cat "$T/versions"
  cleanup_stack
  sleep 2
  if podman ps --filter "name=$ctr" --format '{{.Names}}' | grep -q "$ctr"; then
    bad "stack $id: container not cleaned up"; return 1
  fi
  ok "stack $id: container cleaned up"
  rm -rf "$T"
  [ ! -e "$T" ] && ok "stack $id: state dir cleaned up" || { bad "stack $id: state left behind"; return 1; }
}

[ -x "$HARBOR_BIN" ] || { echo "missing $HARBOR_BIN (run: cargo build --locked)" >&2; exit 1; }
chmod +x "$REPO/tests/gel/toy-chaosbox"
# Self-cleaning: only paths this script owns (unique harbor-db-gel prefix).
rm -f /tmp/harbor-db-gel-stack-*.log
rm -rf /tmp/harbor-db-gel-s[0-9]-?????? 2>/dev/null || true
# Map the image's gel uid to the host user so disposable state stays removable.
GEL_UID="$(podman run --rm "$IMAGE" id -u gel)"
GEL_GID="$(podman run --rm "$IMAGE" id -g gel)"

pids=""
for n in $(seq 1 "$STACKS"); do
  run_stack "s$n" "$((56560 + n))" >"/tmp/harbor-db-gel-stack-s$n.log" 2>&1 &
  pids="$pids $!"
done
rc=0
for pid in $pids; do wait "$pid" || rc=1; done
cat /tmp/harbor-db-gel-stack*.log
grep -h "^not ok" /tmp/harbor-db-gel-stack*.log && rc=1 || true
[ "$rc" -eq 0 ] && echo "ALL GEL INTEGRATION CHECKS PASSED" || echo "GEL INTEGRATION FAILURES PRESENT"
exit "$rc"
