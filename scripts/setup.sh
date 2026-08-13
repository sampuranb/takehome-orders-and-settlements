#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env"
SKIP_BUILD=false

if [[ "${1:-}" == "--skip-build" ]]; then SKIP_BUILD=true; shift; fi
if (($#)); then echo "Usage: ./scripts/setup.sh [--skip-build]" >&2; exit 2; fi

for command in docker openssl curl; do
  command -v "$command" >/dev/null 2>&1 || { echo "Missing required command: $command" >&2; exit 1; }
done
docker compose version >/dev/null 2>&1 || { echo "Docker Compose v2 is required." >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "Docker is installed but is not running." >&2; exit 1; }

cd "$ROOT_DIR"
if [[ ! -f "$ENV_FILE" ]]; then
  umask 077
  cat >"$ENV_FILE" <<EOF
COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-takehome-orders}
AUTH_PORT=${AUTH_PORT:-3005}
APP_PORT=${APP_PORT:-5174}
BETTER_AUTH_SECRET=$(openssl rand -hex 32)
AUTH_DB_PASSWORD=$(openssl rand -hex 24)
APP_DB_PASSWORD=$(openssl rand -hex 24)
EOF
  chmod 600 "$ENV_FILE"
  echo "Generated .env with private random secrets."
else
  chmod 600 "$ENV_FILE"
  echo "Reusing existing .env; no secrets were changed."
fi

compose=(docker compose --env-file "$ENV_FILE" -f "$ROOT_DIR/compose.yaml")
"${compose[@]}" config --quiet
if ! $SKIP_BUILD; then
  echo "Building Better Auth and Orders images..."
  "${compose[@]}" --parallel "${COMPOSE_PARALLEL_LIMIT:-1}" build --pull
fi
"${compose[@]}" up -d --wait --wait-timeout "${COMPOSE_WAIT_TIMEOUT:-1200}"
"$ROOT_DIR/scripts/verify.sh" --env-file "$ENV_FILE" --registration

read_env() { awk -F= -v key="$1" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$ENV_FILE"; }
printf '\nOrders is ready:\n  App:  http://localhost:%s\n  Auth: http://localhost:%s\n' \
  "$(read_env APP_PORT)" "$(read_env AUTH_PORT)"
echo "Docker volumes preserve accounts and application data across restarts."
