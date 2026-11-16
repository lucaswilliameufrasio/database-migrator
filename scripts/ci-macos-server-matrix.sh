#!/usr/bin/env bash
# Disposable PostgreSQL, MySQL, and MariaDB for the macOS fault matrix.
# GitHub service containers run only on Linux. These servers listen on
# 127.0.0.1. The data directory is a case-sensitive volume so MySQL can
# use lower_case_table_names=0, the same setting as the Linux evidence.
set -euo pipefail

prefix="$(brew --prefix)"
pg_bin=""
mysql_bin=""
maria_bin=""
mount="${RUNNER_TEMP}/dbm-servers"
image="${RUNNER_TEMP}/dbm-servers.dmg"
pg_data="${mount}/postgres"
mysql_data="${mount}/mysql"
maria_data="${mount}/mariadb"
mysql_socket="${mount}/mysql.sock"
maria_socket="${mount}/mariadb.sock"
mysql_client="${mount}/mysql-client.cnf"
maria_client="${mount}/mariadb-client.cnf"
mysql_pid=""
maria_pid=""

keg_bin() {
  formula="$1"
  cellar="$(brew --cellar "${formula}")"
  found=""
  for directory in "${cellar}"/*/bin; do
    if [ -d "${directory}" ]; then
      found="${directory}"
    fi
  done
  if [ -z "${found}" ]; then
    echo "no bin directory for ${formula}" >&2
    exit 1
  fi
  printf '%s\n' "${found}"
}

cleanup() {
  if [ -n "${pg_bin}" ] && [ -x "${pg_bin}/pg_ctl" ] && [ -d "${pg_data}" ]; then
    "${pg_bin}/pg_ctl" -D "${pg_data}" -m fast stop >/dev/null 2>&1 || true
  fi
  if [ -n "${mysql_pid}" ]; then
    kill "${mysql_pid}" >/dev/null 2>&1 || true
  fi
  if [ -n "${maria_pid}" ]; then
    kill "${maria_pid}" >/dev/null 2>&1 || true
  fi
  if mount | grep -F " on ${mount} " >/dev/null 2>&1; then
    hdiutil detach "${mount}" >/dev/null 2>&1 || hdiutil detach -force "${mount}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

export HOMEBREW_NO_AUTO_UPDATE=1
export HOMEBREW_NO_ENV_HINTS=1
export HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1

brew install postgresql@18
brew install mysql@8.4
brew unlink mysql@8.4 || true
rm -rf "${prefix}/var/mysql"
brew install mariadb@11.8
brew unlink mariadb@11.8 || true
rm -rf "${prefix}/var/mysql"
pg_bin="$(keg_bin postgresql@18)" || exit 1
mysql_bin="$(keg_bin mysql@8.4)" || exit 1
maria_bin="$(keg_bin mariadb@11.8)" || exit 1
if [ ! -x "${pg_bin}/pg_ctl" ] || [ ! -x "${mysql_bin}/mysqld" ] || [ ! -x "${maria_bin}/mariadbd" ]; then
  echo "a server binary is missing from its keg" >&2
  exit 1
fi

hdiutil create -size 2g -fs 'Case-sensitive APFS' -volname dbm-servers "${image}"
mkdir -p "${mount}"
hdiutil attach "${image}" -nobrowse -mountpoint "${mount}"

mkdir -p "${mount}/probe"
printf 'a\n' >"${mount}/probe/A"
printf 'b\n' >"${mount}/probe/a"
probe_count="$(find "${mount}/probe" -type f | wc -l | tr -d '[:space:]')"
rm -rf "${mount}/probe"
if [ "${probe_count}" != "2" ]; then
  echo "server volume is not case-sensitive" >&2
  exit 1
fi

mkdir -p "${pg_data}" "${mysql_data}" "${maria_data}"
chmod 700 "${pg_data}" "${mysql_data}" "${maria_data}"

"${pg_bin}/initdb" --locale=en_US.UTF-8 --encoding=UTF-8 --username=dbm \
  --auth=trust --auth-host=trust "${pg_data}"
{
  printf "listen_addresses = '127.0.0.1'\n"
  printf "port = 5432\n"
  printf "unix_socket_directories = '%s'\n" "${mount}"
} >>"${pg_data}/postgresql.conf"
"${pg_bin}/pg_ctl" -D "${pg_data}" -l "${mount}/postgres.log" start
"${pg_bin}/createdb" -h 127.0.0.1 -p 5432 -U dbm dbm

umask 077
cat >"${mysql_client}" <<'EOF'
[client]
user=dbm
password=dbm
protocol=TCP
host=127.0.0.1
EOF
cat >"${maria_client}" <<'EOF'
[client]
user=dbm
password=dbm
protocol=TCP
host=127.0.0.1
EOF

"${mysql_bin}/mysqld" \
  --initialize-insecure \
  --datadir="${mysql_data}" \
  --lower_case_table_names=0 \
  --log-error="${mount}/mysql-init.err"
"${mysql_bin}/mysqld" \
  --datadir="${mysql_data}" \
  --bind-address=127.0.0.1 \
  --port=3306 \
  --socket="${mysql_socket}" \
  --mysqlx=0 \
  --skip-name-resolve \
  --lower_case_table_names=0 \
  --pid-file="${mount}/mysql.pid" \
  --log-error="${mount}/mysql.err" &
mysql_pid=$!

"${maria_bin}/mariadb-install-db" --datadir="${maria_data}"
"${maria_bin}/mariadbd" \
  --datadir="${maria_data}" \
  --bind-address=127.0.0.1 \
  --port=3307 \
  --socket="${maria_socket}" \
  --skip-name-resolve \
  --lower_case_table_names=0 \
  --pid-file="${mount}/mariadb.pid" \
  --log-error="${mount}/mariadb.err" &
maria_pid=$!

wait_tcp() {
  port="$1"
  attempt=1
  while [ "${attempt}" -le 90 ]; do
    if python3 -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 2).close()' "${port}"; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  echo "timed out waiting for 127.0.0.1:${port}" >&2
  return 1
}

wait_sql() {
  client="$1"
  socket="$2"
  attempt=1
  while [ "${attempt}" -le 30 ]; do
    if "${client}" --socket="${socket}" -u root -e 'SELECT 1' >/dev/null 2>&1; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  echo "root SQL did not become ready on ${socket}" >&2
  return 1
}

wait_tcp 3306
wait_tcp 3307
wait_sql "${mysql_bin}/mysql" "${mysql_socket}"
wait_sql "${maria_bin}/mariadb" "${maria_socket}"

"${mysql_bin}/mysql" --socket="${mysql_socket}" -u root <<'SQL'
CREATE DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER 'dbm'@'127.0.0.1' IDENTIFIED BY 'dbm';
GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'127.0.0.1' WITH GRANT OPTION;
FLUSH PRIVILEGES;
SQL

"${maria_bin}/mariadb" --socket="${maria_socket}" -u root <<'SQL'
CREATE DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER 'dbm'@'127.0.0.1' IDENTIFIED BY 'dbm';
GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'127.0.0.1' WITH GRANT OPTION;
FLUSH PRIVILEGES;
SQL

pg_version="$("${pg_bin}/psql" -h 127.0.0.1 -p 5432 -U dbm -d dbm -Atc 'SHOW server_version')"
mysql_version="$("${mysql_bin}/mysql" --defaults-extra-file="${mysql_client}" -P 3306 -N -e 'SELECT VERSION()')"
maria_version="$("${maria_bin}/mariadb" --defaults-extra-file="${maria_client}" -P 3307 -N -e 'SELECT VERSION()')"
printf 'postgres_version=%s\n' "${pg_version}"
printf 'mysql_version=%s\n' "${mysql_version}"
printf 'mariadb_version=%s\n' "${maria_version}"

case "${pg_version}" in
  18.*) ;;
  *)
    echo "PostgreSQL is not version 18" >&2
    exit 1
    ;;
esac
case "${mysql_version}" in
  8.4.*) ;;
  *)
    echo "MySQL is not version 8.4" >&2
    exit 1
    ;;
esac
maria_lower="$(printf '%s' "${maria_version}" | tr '[:upper:]' '[:lower:]')"
case "${maria_lower}" in
  *mariadb*) ;;
  *)
    echo "MariaDB did not report itself" >&2
    exit 1
    ;;
esac
case "${maria_version}" in
  11.8.*) ;;
  *)
    echo "MariaDB is not version 11.8" >&2
    exit 1
    ;;
esac

export DBM_TEST_DATABASE_URL='postgres://dbm@127.0.0.1:5432/dbm?sslmode=disable'
export DBM_TEST_MYSQL_URL='mysql://dbm:dbm@127.0.0.1:3306/dbm'
export DBM_TEST_MARIADB_URL='mysql://dbm:dbm@127.0.0.1:3307/dbm'

cargo test --workspace --all-features --locked -- \
  --ignored --skip pgbouncer --skip _tls_ --test-threads=1
