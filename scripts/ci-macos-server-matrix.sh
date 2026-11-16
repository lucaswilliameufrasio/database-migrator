#!/usr/bin/env bash
# Disposable PostgreSQL, MySQL, and MariaDB for one macOS fault-matrix cell.
# GitHub service containers run only on Linux. These servers listen on
# 127.0.0.1. The data directory is a case-sensitive volume so MySQL can
# use lower_case_table_names=0, the same setting as the Linux evidence.
#
# The workflow selects one declared line of each server. Homebrew bottles
# checked on 2026-11-16 were postgresql@16 16.16, postgresql@17 17.12,
# postgresql@18 18.7, mysql@8.0 8.0.46, mysql@8.4 8.4.13, mysql@9.7 9.7.4,
# mariadb@10.11 10.11.19, mariadb@11.4 11.4.13, mariadb@11.8 11.8.9,
# mariadb@12.3 12.3.3, and mariadb 13.0.2 (installable as the mariadb@13.0
# alias). MySQL 8.0 left Oracle support on 2026-04-30. MariaDB 12.3 is an
# LTS line. The crate accepts both, and one cell exercises them.
set -euo pipefail

postgres_formula="${DBM_MACOS_POSTGRES_FORMULA:-postgresql@18}"
postgres_prefix="${DBM_MACOS_POSTGRES_PREFIX:-18.}"
mysql_formula="${DBM_MACOS_MYSQL_FORMULA:-mysql@8.4}"
mysql_prefix="${DBM_MACOS_MYSQL_PREFIX:-8.4.}"
mariadb_formula="${DBM_MACOS_MARIADB_FORMULA:-mariadb@11.8}"
mariadb_prefix="${DBM_MACOS_MARIADB_PREFIX:-11.8.}"

require_choice() {
  kind="$1"
  value="$2"
  case "${kind}" in
    formula)
      case "${value}" in
        postgresql@16|postgresql@17|postgresql@18|mysql@8.0|mysql@8.4|mysql@9.7|mariadb@10.11|mariadb@11.4|mariadb@11.8|mariadb@12.3|mariadb|mariadb@13.0)
          ;;
        *)
          echo "refusing formula ${value}" >&2
          exit 1
          ;;
      esac
      ;;
    prefix)
      case "${value}" in
        16.|17.|18.|8.0.|8.4.|9.7.|10.11.|11.4.|11.8.|12.3.|13.0.)
          ;;
        *)
          echo "refusing version prefix ${value}" >&2
          exit 1
          ;;
      esac
      ;;
    *)
      echo "unknown choice kind ${kind}" >&2
      exit 1
      ;;
  esac
}

require_choice formula "${postgres_formula}"
require_choice prefix "${postgres_prefix}"
require_choice formula "${mysql_formula}"
require_choice prefix "${mysql_prefix}"
require_choice formula "${mariadb_formula}"
require_choice prefix "${mariadb_prefix}"

case "${postgres_formula}" in
  postgresql@16|postgresql@17|postgresql@18) ;;
  *)
    echo "postgres formula is ${postgres_formula}" >&2
    exit 1
    ;;
esac
case "${mysql_formula}" in
  mysql@8.0|mysql@8.4|mysql@9.7) ;;
  *)
    echo "mysql formula is ${mysql_formula}" >&2
    exit 1
    ;;
esac
case "${mariadb_formula}" in
  mariadb@10.11|mariadb@11.4|mariadb@11.8|mariadb@12.3|mariadb|mariadb@13.0) ;;
  *)
    echo "mariadb formula is ${mariadb_formula}" >&2
    exit 1
    ;;
esac

prefix="$(brew --prefix)"
pg_bin=""
mysql_pid=""
maria_pid=""
mount="${RUNNER_TEMP}/dbm-servers"
image="${RUNNER_TEMP}/dbm-servers.dmg"
pg_data="${mount}/postgres"
mysql_data="${mount}/mysql"
maria_data="${mount}/mariadb"
mysql_socket="${mount}/mysql.sock"
maria_socket="${mount}/mariadb.sock"
mysql_tmp="${mount}/mysql-tmp"
maria_tmp="${mount}/mariadb-tmp"
mysql_client="${mount}/mysql-client.cnf"
maria_client="${mount}/mariadb-client.cnf"
pg_ctl=""
mysql_server=""
mysql_cli=""
maria_server=""
maria_install=""
maria_cli=""

canonical_formula() {
  formula="$1"
  brew info --json=v2 "${formula}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["formulae"][0]["name"])'
}

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

first_executable() {
  directory="$1"
  shift
  name=""
  for name in "$@"; do
    if [ -x "${directory}/${name}" ]; then
      printf '%s\n' "${directory}/${name}"
      return 0
    fi
  done
  echo "missing executable in ${directory}" >&2
  return 1
}

cleanup() {
  if [ -n "${pg_ctl}" ] && [ -x "${pg_ctl}" ] && [ -d "${pg_data}" ]; then
    "${pg_ctl}" -D "${pg_data}" -m fast stop >/dev/null 2>&1 || true
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

printf 'postgres_formula=%s prefix=%s\n' "${postgres_formula}" "${postgres_prefix}"
printf 'mysql_formula=%s prefix=%s\n' "${mysql_formula}" "${mysql_prefix}"
printf 'mariadb_formula=%s prefix=%s\n' "${mariadb_formula}" "${mariadb_prefix}"

# The runner image can lag the Homebrew tap. Update once, and only when a
# requested formula is missing, so a new line such as MariaDB 13.0 is
# installable without moving the other bottles on every run.
brew_updated=no
ensure_formula() {
  formula="$1"
  if brew info --json=v2 "${formula}" >/dev/null 2>&1; then
    return 0
  fi
  if [ "${brew_updated}" != "yes" ]; then
    brew update
    brew_updated=yes
  fi
  if ! brew info --json=v2 "${formula}" >/dev/null 2>&1; then
    echo "formula ${formula} is not in Homebrew after update" >&2
    exit 1
  fi
}

ensure_formula "${postgres_formula}"
ensure_formula "${mysql_formula}"
ensure_formula "${mariadb_formula}"
postgres_canonical="$(canonical_formula "${postgres_formula}")"
mysql_canonical="$(canonical_formula "${mysql_formula}")"
mariadb_canonical="$(canonical_formula "${mariadb_formula}")"

brew install "${postgres_formula}"
brew install "${mysql_formula}"
brew unlink "${mysql_canonical}" || true
rm -rf "${prefix}/var/mysql"
# mysql@8.4 writes mysqlx-bind-address into the shared Homebrew my.cnf.
# MariaDB aborts on that variable, including in the brew postinstall.
# Servers below pass --no-defaults so they never read this file.
if [ -f "${prefix}/etc/my.cnf" ]; then
  mv "${prefix}/etc/my.cnf" "${prefix}/etc/my.cnf.mysql-aside"
  printf 'moved Homebrew my.cnf to %s\n' "${prefix}/etc/my.cnf.mysql-aside"
fi
if ! brew install "${mariadb_formula}"; then
  maria_ready=""
  maria_cellar="$(brew --cellar "${mariadb_canonical}")"
  for directory in "${maria_cellar}"/*/bin; do
    if [ -x "${directory}/mariadbd" ] || [ -x "${directory}/mysqld" ]; then
      maria_ready="${directory}"
    fi
  done
  if [ -z "${maria_ready}" ]; then
    echo "mariadb bottle did not install" >&2
    exit 1
  fi
  printf 'mariadb postinstall failed; continuing because %s exists\n' "${maria_ready}" >&2
fi
brew unlink "${mariadb_canonical}" || true
rm -rf "${prefix}/var/mysql"
pg_bin="$(keg_bin "${postgres_canonical}")" || exit 1
mysql_bin="$(keg_bin "${mysql_canonical}")" || exit 1
maria_bin="$(keg_bin "${mariadb_canonical}")" || exit 1
pg_ctl="$(first_executable "${pg_bin}" pg_ctl)" || exit 1
pg_initdb="$(first_executable "${pg_bin}" initdb)" || exit 1
pg_createdb="$(first_executable "${pg_bin}" createdb)" || exit 1
pg_psql="$(first_executable "${pg_bin}" psql)" || exit 1
mysql_server="$(first_executable "${mysql_bin}" mysqld)" || exit 1
mysql_cli="$(first_executable "${mysql_bin}" mysql)" || exit 1
maria_server="$(first_executable "${maria_bin}" mariadbd mysqld)" || exit 1
maria_install="$(first_executable "${maria_bin}" mariadb-install-db mysql_install_db)" || exit 1
maria_cli="$(first_executable "${maria_bin}" mariadb mysql)" || exit 1

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

mkdir -p "${pg_data}" "${mysql_data}" "${maria_data}" "${mysql_tmp}" "${maria_tmp}"
chmod 700 "${pg_data}" "${mysql_data}" "${maria_data}" "${mysql_tmp}" "${maria_tmp}"

"${pg_initdb}" --locale=en_US.UTF-8 --encoding=UTF-8 --username=dbm \
  --auth=trust --auth-host=trust "${pg_data}"
{
  printf "listen_addresses = '127.0.0.1'\n"
  printf "port = 5432\n"
  printf "unix_socket_directories = '%s'\n" "${mount}"
} >>"${pg_data}/postgresql.conf"
"${pg_ctl}" -D "${pg_data}" -l "${mount}/postgres.log" start
"${pg_createdb}" -h 127.0.0.1 -p 5432 -U dbm dbm

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

if ! "${mysql_server}" \
  --no-defaults \
  --initialize-insecure \
  --datadir="${mysql_data}" \
  --tmpdir="${mysql_tmp}" \
  --lower_case_table_names=0 \
  --log-error="${mount}/mysql-init.err"; then
  echo "mysql initialize failed" >&2
  if [ -f "${mount}/mysql-init.err" ]; then
    tail -n 50 "${mount}/mysql-init.err" >&2
  fi
  exit 1
fi
"${mysql_server}" \
  --no-defaults \
  --datadir="${mysql_data}" \
  --tmpdir="${mysql_tmp}" \
  --bind-address=127.0.0.1 \
  --port=3306 \
  --socket="${mysql_socket}" \
  --mysqlx=0 \
  --skip-name-resolve \
  --lower_case_table_names=0 \
  --pid-file="${mount}/mysql.pid" \
  --log-error="${mount}/mysql.err" &
mysql_pid=$!

# The default is unix_socket. root@localhost then accepts only the OS root
# user, and the GitHub runner is not root. An empty password lets the
# disposable account be created over the local socket.
# The runner can delete a file in its default temp directory while bootstrap
# still needs it, and MariaDB then aborts on mysql.tmp_user_sys. The temp
# directory stays on this volume, beside the datadir: initialize refuses a
# datadir that is not empty. One fresh retry covers a partial init.
install_maria() {
  "${maria_install}" \
    --no-defaults \
    --datadir="${maria_data}" \
    --tmpdir="${maria_tmp}" \
    --auth-root-authentication-method=normal
}
if ! install_maria; then
  echo "mariadb-install-db failed; retrying once with a fresh datadir" >&2
  rm -rf "${maria_data}" "${maria_tmp}"
  mkdir -p "${maria_data}" "${maria_tmp}"
  chmod 700 "${maria_data}" "${maria_tmp}"
  install_maria
fi
"${maria_server}" \
  --no-defaults \
  --datadir="${maria_data}" \
  --tmpdir="${maria_tmp}" \
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
  error_log="$2"
  attempt=1
  while [ "${attempt}" -le 90 ]; do
    if python3 -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 2).close()' "${port}" >/dev/null 2>&1; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  echo "timed out waiting for 127.0.0.1:${port}" >&2
  if [ -f "${error_log}" ]; then
    tail -n 50 "${error_log}" >&2
  fi
  return 1
}

wait_sql() {
  client="$1"
  socket="$2"
  error_log="$3"
  probe_log="${mount}/sql-probe.err"
  attempt=1
  while [ "${attempt}" -le 30 ]; do
    if "${client}" --no-defaults --socket="${socket}" -u root -e 'SELECT 1' >/dev/null 2>"${probe_log}"; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  echo "root SQL did not become ready on ${socket}" >&2
  if [ -s "${probe_log}" ]; then
    cat "${probe_log}" >&2
  fi
  if [ -f "${error_log}" ]; then
    tail -n 50 "${error_log}" >&2
  fi
  return 1
}

wait_tcp 3306 "${mount}/mysql.err"
wait_tcp 3307 "${mount}/mariadb.err"
wait_sql "${mysql_cli}" "${mysql_socket}" "${mount}/mysql.err"
wait_sql "${maria_cli}" "${maria_socket}" "${mount}/mariadb.err"

"${mysql_cli}" --no-defaults --socket="${mysql_socket}" -u root <<'SQL'
CREATE DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER 'dbm'@'127.0.0.1' IDENTIFIED BY 'dbm';
GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'127.0.0.1' WITH GRANT OPTION;
FLUSH PRIVILEGES;
SQL

"${maria_cli}" --no-defaults --socket="${maria_socket}" -u root <<'SQL'
CREATE DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER 'dbm'@'127.0.0.1' IDENTIFIED BY 'dbm';
GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'127.0.0.1' WITH GRANT OPTION;
FLUSH PRIVILEGES;
SQL

pg_version="$("${pg_psql}" -h 127.0.0.1 -p 5432 -U dbm -d dbm -Atc 'SHOW server_version')"
mysql_version="$("${mysql_cli}" --defaults-file="${mysql_client}" -P 3306 -N -e 'SELECT VERSION()')"
maria_version="$("${maria_cli}" --defaults-file="${maria_client}" -P 3307 -N -e 'SELECT VERSION()')"
printf 'postgres_version=%s\n' "${pg_version}"
printf 'mysql_version=%s\n' "${mysql_version}"
printf 'mariadb_version=%s\n' "${maria_version}"

require_version_prefix() {
  label="$1"
  version="$2"
  expected="$3"
  case "${version}" in
    "${expected}"*) ;;
    *)
      echo "${label} ${version} does not start with ${expected}" >&2
      exit 1
      ;;
  esac
}

require_version_prefix PostgreSQL "${pg_version}" "${postgres_prefix}"
require_version_prefix MySQL "${mysql_version}" "${mysql_prefix}"
maria_lower="$(printf '%s' "${maria_version}" | tr '[:upper:]' '[:lower:]')"
case "${maria_lower}" in
  *mariadb*) ;;
  *)
    echo "MariaDB did not report itself" >&2
    exit 1
    ;;
esac
require_version_prefix MariaDB "${maria_version}" "${mariadb_prefix}"

export DBM_TEST_DATABASE_URL='postgres://dbm@127.0.0.1:5432/dbm?sslmode=disable'
export DBM_TEST_MYSQL_URL='mysql://dbm:dbm@127.0.0.1:3306/dbm'
export DBM_TEST_MARIADB_URL='mysql://dbm:dbm@127.0.0.1:3307/dbm'

cargo test --workspace --all-features --locked -- \
  --ignored --skip pgbouncer --skip _tls_ --test-threads=1
