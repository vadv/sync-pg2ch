#!/usr/bin/env bash
set -euo pipefail

: "${PG_CONTAINER:?PostgreSQL service container is required}"
: "${CH_CONTAINER:?ClickHouse service container is required}"
: "${TEST_PG_PORT:?PostgreSQL service port is required}"
: "${TEST_CH_HTTPS_PORT:?ClickHouse HTTPS service port is required}"
: "${GITHUB_ENV:?GitHub environment file is required}"

tls_dir="$(mktemp -d)"
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 1 \
  -keyout "$tls_dir/ca.key" -out "$tls_dir/ca.crt" \
  -subj '/CN=sync-pg2ch integration CA' \
  -addext 'basicConstraints=critical,CA:TRUE'
openssl req -new -newkey rsa:2048 -nodes -sha256 \
  -keyout "$tls_dir/server.key" -out "$tls_dir/server.csr" \
  -subj '/CN=localhost'
cat > "$tls_dir/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EOF
openssl x509 -req -in "$tls_dir/server.csr" -CA "$tls_dir/ca.crt" \
  -CAkey "$tls_dir/ca.key" -CAcreateserial -out "$tls_dir/server.crt" \
  -days 1 -sha256 -extfile "$tls_dir/server.ext"

docker exec -u 0 "$PG_CONTAINER" mkdir -p /tmp/pg2ch-test-tls
docker cp "$tls_dir/server.crt" "$PG_CONTAINER:/tmp/pg2ch-test-tls/server.crt"
docker cp "$tls_dir/server.key" "$PG_CONTAINER:/tmp/pg2ch-test-tls/server.key"
docker exec -u 0 "$PG_CONTAINER" chown -R postgres:postgres /tmp/pg2ch-test-tls
docker exec -u 0 "$PG_CONTAINER" chmod 600 /tmp/pg2ch-test-tls/server.key
docker exec -i "$PG_CONTAINER" psql -U postgres -d postgres -v ON_ERROR_STOP=1 <<'SQL'
ALTER SYSTEM SET ssl = 'on';
ALTER SYSTEM SET ssl_cert_file = '/tmp/pg2ch-test-tls/server.crt';
ALTER SYSTEM SET ssl_key_file = '/tmp/pg2ch-test-tls/server.key';
SELECT pg_reload_conf();
SQL

docker exec -u 0 "$CH_CONTAINER" mkdir -p /tmp/pg2ch-test-tls
docker cp "$tls_dir/server.crt" "$CH_CONTAINER:/tmp/pg2ch-test-tls/server.crt"
docker cp "$tls_dir/server.key" "$CH_CONTAINER:/tmp/pg2ch-test-tls/server.key"
docker exec -u 0 "$CH_CONTAINER" chown -R clickhouse:clickhouse /tmp/pg2ch-test-tls
docker exec -u 0 "$CH_CONTAINER" chmod 600 /tmp/pg2ch-test-tls/server.key
cat > "$tls_dir/https.xml" <<'XML'
<clickhouse>
    <https_port>8443</https_port>
    <openSSL>
        <server>
            <certificateFile>/tmp/pg2ch-test-tls/server.crt</certificateFile>
            <privateKeyFile>/tmp/pg2ch-test-tls/server.key</privateKeyFile>
            <verificationMode>none</verificationMode>
        </server>
    </openSSL>
</clickhouse>
XML
docker cp "$tls_dir/https.xml" "$CH_CONTAINER:/etc/clickhouse-server/config.d/test-https.xml"
docker restart "$CH_CONTAINER"

ready=false
for attempt in $(seq 1 30); do
  if curl --fail --silent --cacert "$tls_dir/ca.crt" \
      "https://localhost:$TEST_CH_HTTPS_PORT/ping" > /dev/null; then
    ready=true
    break
  fi
  sleep 1
done
if [ "$ready" != true ]; then
  echo 'ClickHouse HTTPS listener did not become ready' >&2
  exit 1
fi

printf 'TEST_PG_TLS_URL=postgres://postgres:test-pass@localhost:%s/postgres?sslmode=verify-full&sslrootcert=%s/ca.crt\n' "$TEST_PG_PORT" "$tls_dir" >> "$GITHUB_ENV"
printf 'TEST_CH_TLS_URL=https://localhost:%s\n' "$TEST_CH_HTTPS_PORT" >> "$GITHUB_ENV"
printf 'TEST_CH_CA_CERT=%s/ca.crt\n' "$tls_dir" >> "$GITHUB_ENV"
