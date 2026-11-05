#!/usr/bin/env bash
set -euo pipefail

: "${PG_CONTAINER:?}" "${CH_CONTAINER:?}" "${SYNC_PG2CH_TEST_BINARY:?}"
socket_database="socket_test_$$"
socket_config=$(mktemp)
socket_container_config="/tmp/${socket_database}.toml"
socket_container_binary="/tmp/${socket_database}"
socket_ch_address=$(docker inspect --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$CH_CONTAINER")

ch_query() {
    docker exec "$CH_CONTAINER" clickhouse-client --user default --password test-pass --query "$1"
}

cleanup() {
    docker exec -u postgres "$PG_CONTAINER" dropdb --if-exists "$socket_database" || true
    ch_query "DROP DATABASE IF EXISTS $socket_database" || true
    docker exec "$PG_CONTAINER" rm -f "$socket_container_config" "$socket_container_binary" || true
    rm -f "$socket_config"
}
trap cleanup EXIT

docker exec -u postgres "$PG_CONTAINER" createdb "$socket_database"
docker exec -u postgres "$PG_CONTAINER" psql -v ON_ERROR_STOP=1 -d "$socket_database" \
    -c 'CREATE TABLE socket_case (id integer NOT NULL)'
ch_query "CREATE DATABASE $socket_database"
ch_query "CREATE TABLE $socket_database.socket_case (id Int32) ENGINE = MergeTree ORDER BY id"

cat > "$socket_config" <<EOF
config_version = 1
[postgres]
url_env = "PG_URL"
[clickhouse]
url_env = "CH_URL"
database = "$socket_database"
user_env = "CH_USER"
password_env = "CH_PASSWORD"
EOF
chmod 644 "$socket_config"
docker cp "$socket_config" "$PG_CONTAINER:$socket_container_config"
docker cp "$SYNC_PG2CH_TEST_BINARY" "$PG_CONTAINER:$socket_container_binary"
docker exec "$PG_CONTAINER" chmod 755 "$socket_container_binary"
docker exec -u postgres \
    -e "PG_URL=postgres://postgres@/$socket_database?host=/var/run/postgresql&sslmode=disable" \
    -e "CH_URL=http://$socket_ch_address:8123" -e CH_USER=default -e CH_PASSWORD=test-pass \
    "$PG_CONTAINER" "$socket_container_binary" diff --config "$socket_container_config"
