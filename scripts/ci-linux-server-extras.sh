#!/usr/bin/env bash
# Disposable loopback servers for the gaps the macOS matrix skips:
# the ignored suite without --test-threads=1, PgBouncer transaction pooling,
# and the three TLS tests. PostgreSQL 18, MySQL 8.4, and MariaDB 11.8 are
# the lines under those extra checks. MySQL 8.0 and MariaDB 12.3 stay on
# the macOS cell that selects those formulas.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "${root}"

work="${RUNNER_TEMP}/dbm-linux-extras"
rm -rf "${work}"
mkdir -p "${work}/certs"
certs="${work}/certs"

cleanup() {
  docker rm -f dbm-pg dbm-mysql dbm-maria dbm-pg-tls dbm-mysql-tls dbm-maria-tls >/dev/null 2>&1 || true
  if [ -n "${pgbouncer_pid:-}" ]; then
    kill "${pgbouncer_pid}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

docker rm -f dbm-pg dbm-mysql dbm-maria dbm-pg-tls dbm-mysql-tls dbm-maria-tls >/dev/null 2>&1 || true

echo "starting plaintext servers"
docker run -d --name dbm-pg \
  -p 127.0.0.1:5432:5432 \
  -e POSTGRES_USER=dbm \
  -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e POSTGRES_DB=dbm \
  postgres:18 \
  -c max_connections=200 \
  -c listen_addresses='*'
docker run -d --name dbm-mysql \
  -p 127.0.0.1:3306:3306 \
  -e MYSQL_ROOT_PASSWORD=dbm \
  -e MYSQL_USER=dbm \
  -e MYSQL_PASSWORD=dbm \
  -e MYSQL_DATABASE=dbm \
  mysql:8.4 \
  --bind-address=0.0.0.0 \
  --max-connections=300 \
  --lower-case-table-names=0
docker run -d --name dbm-maria \
  -p 127.0.0.1:3307:3306 \
  -e MARIADB_ROOT_PASSWORD=dbm \
  -e MARIADB_USER=dbm \
  -e MARIADB_PASSWORD=dbm \
  -e MARIADB_DATABASE=dbm \
  mariadb:11.8 \
  --bind-address=0.0.0.0 \
  --max-connections=300 \
  --lower-case-table-names=0

wait_exec() {
  name="$1"
  shift
  attempt=1
  while [ "${attempt}" -le 90 ]; do
    if docker exec "${name}" "$@" >/dev/null 2>&1; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 2
  done
  echo "timed out waiting for ${name}" >&2
  docker logs "${name}" >&2 || true
  return 1
}

wait_exec dbm-pg pg_isready -U dbm
wait_exec dbm-mysql mysqladmin ping -uroot -pdbm --silent
wait_exec dbm-maria mariadb-admin ping -uroot -pdbm --silent

docker exec dbm-mysql mysql -uroot -pdbm -e "ALTER DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin; GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'%' WITH GRANT OPTION; FLUSH PRIVILEGES;"
docker exec dbm-maria mariadb -uroot -pdbm -e "ALTER DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin; GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'%' WITH GRANT OPTION; FLUSH PRIVILEGES;"

export DBM_TEST_DATABASE_URL='postgres://dbm@127.0.0.1:5432/dbm?sslmode=disable'
export DBM_TEST_MYSQL_URL='mysql://dbm:dbm@127.0.0.1:3306/dbm'
export DBM_TEST_MARIADB_URL='mysql://dbm:dbm@127.0.0.1:3307/dbm'
printf 'postgres_version=%s\n' "$(docker exec dbm-pg psql -U dbm -d dbm -Atc 'SHOW server_version')"
printf 'mysql_version=%s\n' "$(docker exec dbm-mysql mysql -udbm -pdbm -N -e 'SELECT VERSION()')"
printf 'mariadb_version=%s\n' "$(docker exec dbm-maria mariadb -udbm -pdbm -N -e 'SELECT VERSION()')"

echo "running the ignored suite with the default thread count"
cargo test --workspace --all-features --locked --no-fail-fast -- \
  --ignored --skip pgbouncer --skip _tls_

echo "starting PgBouncer"
sudo DEBIAN_FRONTEND=noninteractive apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y pgbouncer postgresql-client
sudo systemctl stop pgbouncer || true
cat > "${work}/userlist.txt" <<'EOF'
"dbm" ""
EOF
cat > "${work}/pgbouncer.ini" <<'EOF'
[databases]
dbm = host=127.0.0.1 port=5432 dbname=dbm

[pgbouncer]
listen_addr = 127.0.0.1
listen_port = 6432
auth_type = trust
auth_file = /tmp/dbm-userlist.txt
pool_mode = transaction
default_pool_size = 2
min_pool_size = 0
reserve_pool_size = 0
server_reset_query = ''
ignore_startup_parameters = extra_float_digits
admin_users = dbm
stats_users = dbm
EOF
cp "${work}/userlist.txt" /tmp/dbm-userlist.txt
pgbouncer "${work}/pgbouncer.ini" &
pgbouncer_pid=$!
attempt=1
while [ "${attempt}" -le 30 ]; do
  if pg_isready -h 127.0.0.1 -p 6432 -U dbm >/dev/null 2>&1; then
    break
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if ! pg_isready -h 127.0.0.1 -p 6432 -U dbm >/dev/null 2>&1; then
  echo "PgBouncer did not accept connections" >&2
  exit 1
fi
export DBM_TEST_PGBOUNCER_URL='postgres://dbm@127.0.0.1:6432/dbm?sslmode=disable'
cargo test --workspace --all-features --locked -- --ignored pgbouncer --test-threads=1

echo "starting TLS servers"
cat > "${certs}/san.cnf" <<'EOF'
[ext]
subjectAltName = IP:127.0.0.1
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
EOF
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "${certs}/ca.key" -out "${certs}/ca.pem" -days 2 \
  -subj '/CN=dbm-test-ca' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign'
openssl req -newkey rsa:2048 -nodes \
  -keyout "${certs}/server.key" -out "${certs}/server.csr" \
  -subj '/CN=127.0.0.1'
openssl x509 -req -in "${certs}/server.csr" \
  -CA "${certs}/ca.pem" -CAkey "${certs}/ca.key" -CAcreateserial \
  -out "${certs}/server.crt" -days 2 \
  -extfile "${certs}/san.cnf" -extensions ext
openssl verify -CAfile "${certs}/ca.pem" "${certs}/server.crt"
sudo chown 999:999 "${certs}/server.key"
sudo chmod 600 "${certs}/server.key"

docker run -d --name dbm-pg-tls \
  -p 127.0.0.1:5433:5432 \
  -v "${certs}:/certs:ro" \
  -e POSTGRES_USER=dbm \
  -e POSTGRES_HOST_AUTH_METHOD=trust \
  -e POSTGRES_DB=dbm \
  postgres:18 \
  -c ssl=on \
  -c ssl_cert_file=/certs/server.crt \
  -c ssl_key_file=/certs/server.key
docker run -d --name dbm-mysql-tls \
  -p 127.0.0.1:3308:3306 \
  -v "${certs}:/certs:ro" \
  -e MYSQL_ROOT_PASSWORD=dbm \
  -e MYSQL_USER=dbm \
  -e MYSQL_PASSWORD=dbm \
  -e MYSQL_DATABASE=dbm \
  mysql:8.4 \
  --bind-address=0.0.0.0 \
  --ssl-cert=/certs/server.crt \
  --ssl-key=/certs/server.key \
  --ssl-ca=/certs/ca.pem
docker run -d --name dbm-maria-tls \
  -p 127.0.0.1:3309:3306 \
  -v "${certs}:/certs:ro" \
  -e MARIADB_ROOT_PASSWORD=dbm \
  -e MARIADB_USER=dbm \
  -e MARIADB_PASSWORD=dbm \
  -e MARIADB_DATABASE=dbm \
  mariadb:11.8 \
  --bind-address=0.0.0.0 \
  --ssl-cert=/certs/server.crt \
  --ssl-key=/certs/server.key \
  --ssl-ca=/certs/ca.pem

wait_exec dbm-pg-tls pg_isready -U dbm
wait_exec dbm-mysql-tls mysqladmin ping -uroot -pdbm --silent
wait_exec dbm-maria-tls mariadb-admin ping -uroot -pdbm --silent

export DBM_TEST_DATABASE_URL='postgres://dbm@127.0.0.1:5433/dbm?sslmode=require'
export DBM_TEST_POSTGRES_CA_FILE="${certs}/ca.pem"
export DBM_TEST_MYSQL_TLS_URL='mysql://dbm:dbm@127.0.0.1:3308/dbm?require_ssl=true'
export DBM_TEST_MYSQL_CA_FILE="${certs}/ca.pem"
export DBM_TEST_MARIADB_TLS_URL='mysql://dbm:dbm@127.0.0.1:3309/dbm?require_ssl=true'
export DBM_TEST_MARIADB_CA_FILE="${certs}/ca.pem"
if ! cargo test --workspace --all-features --locked --no-fail-fast -- --ignored _tls_ --test-threads=1; then
  echo "TLS tests failed; container logs follow" >&2
  docker logs dbm-pg-tls >&2 || true
  docker logs dbm-mysql-tls >&2 || true
  docker logs dbm-maria-tls >&2 || true
  exit 1
fi
