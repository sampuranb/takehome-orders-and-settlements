#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env"
CHECK_REGISTRATION=false
while (($#)); do
  case "$1" in
    --env-file) ENV_FILE="$2"; shift ;;
    --registration) CHECK_REGISTRATION=true ;;
    *) echo "Usage: ./scripts/verify.sh [--env-file PATH] [--registration]" >&2; exit 2 ;;
  esac
  shift
done

read_env() { awk -F= -v key="$1" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$ENV_FILE"; }
wait_for_url() {
  local label="$1" url="$2"
  for _ in $(seq 1 90); do
    if curl --fail --silent --show-error --output /dev/null "$url"; then printf '  %-8s OK\n' "$label"; return; fi
    sleep 2
  done
  echo "$label did not become ready at $url" >&2; return 1
}

cd "$ROOT_DIR"
compose=(docker compose --env-file "$ENV_FILE" -f "$ROOT_DIR/compose.yaml")
auth_url="http://localhost:$(read_env AUTH_PORT)"
app_url="http://localhost:$(read_env APP_PORT)"
echo "Checking service readiness..."
wait_for_url Auth "$auth_url/health"
wait_for_url Orders "$app_url/health"

for service in auth app; do
  id="$("${compose[@]}" ps -q "$service")"
  [[ -n "$id" && "$(docker inspect --format '{{.State.Running}}' "$id")" == true ]] || { echo "$service is not running." >&2; exit 1; }
done

if $CHECK_REGISTRATION; then
  temp_dir="$(mktemp -d)"; cookie_jar="$temp_dir/cookies"; response_file="$temp_dir/signup.json"
  email="verify.$(date +%s).$RANDOM@example.test"; password="Verify-$RANDOM-$RANDOM!!"
  cleanup() { rm -f "$cookie_jar" "$response_file"; rmdir "$temp_dir" 2>/dev/null || true; }
  trap cleanup EXIT
  status="$(curl --silent --show-error --output "$response_file" --cookie-jar "$cookie_jar" --write-out '%{http_code}' \
    --request POST "$auth_url/api/auth/sign-up/email" --header "Origin: $app_url" --header 'Content-Type: application/json' \
    --data "{\"name\":\"Compose Verification\",\"email\":\"$email\",\"password\":\"$password\"}")"
  [[ "$status" == 200 ]] && grep -Fq "\"email\":\"$email\"" "$response_file" || { echo "Signup failed with HTTP $status." >&2; exit 1; }
  curl --fail --silent --show-error --cookie "$cookie_jar" "$auth_url/api/auth/get-session" | grep -Fq "\"email\":\"$email\"" || { echo "Session check failed." >&2; exit 1; }
  "${compose[@]}" exec -T auth-db psql -v ON_ERROR_STOP=1 -U auth -d auth -c "DELETE FROM \"user\" WHERE email = '$email';" >/dev/null
  echo "  Signup   OK (temporary account removed)"
fi
echo "All checks passed."
