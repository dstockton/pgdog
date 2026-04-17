#!/bin/bash
# Run quota enforcement integration tests.
#
# Usage:
#   ./run.sh          # run tests and clean up
#   ./run.sh --keep   # keep containers running after tests (for debugging)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
cd "$SCRIPT_DIR"

KEEP=false
if [ "$1" = "--keep" ]; then
    KEEP=true
fi

echo "Building PgDog and starting containers..."
docker compose build --quiet
docker compose up -d tenant_a tenant_b pgdog

echo "Waiting for services to be healthy..."
docker compose up --exit-code-from test test
EXIT_CODE=$?

if [ "$KEEP" = false ]; then
    echo "Cleaning up..."
    docker compose down -v --remove-orphans
fi

exit $EXIT_CODE
