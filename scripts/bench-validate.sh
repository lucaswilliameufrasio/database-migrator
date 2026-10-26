#!/bin/sh
set -eu

usage() {
    printf 'Usage: sh scripts/bench-validate.sh [dbm-binary] [migration-count] [samples]\n' >&2
    exit 2
}

if [ "$#" -gt 3 ]; then
    usage
fi

binary=${1:-target/release/dbm}
migration_count=${2:-1000}
samples=${3:-5}

case "$migration_count" in
    ''|*[!0-9]*) usage ;;
esac
case "$samples" in
    ''|*[!0-9]*) usage ;;
esac
if [ "$migration_count" -lt 1 ] || [ "$migration_count" -gt 10000 ]; then
    usage
fi
if [ "$samples" -lt 1 ] || [ "$samples" -gt 20 ]; then
    usage
fi

case "$binary" in
    /*) ;;
    *) binary="$PWD/$binary" ;;
esac
if [ ! -x "$binary" ]; then
    printf 'dbm binary is not executable: %s\n' "$binary" >&2
    exit 2
fi

scratch_root=${DBM_BENCH_TMPDIR:-/tmp/opencode}
if [ ! -d "$scratch_root" ] || [ ! -w "$scratch_root" ]; then
    printf 'scratch root must exist and be writable: %s\n' "$scratch_root" >&2
    exit 2
fi
scratch_root=$(CDPATH= cd "$scratch_root" && pwd -P)

workdir=$(mktemp -d "$scratch_root/dbm-validate-bench.XXXXXX")
cleanup() {
    case "$workdir" in
        "$scratch_root"/dbm-validate-bench.*) rm -rf -- "$workdir" ;;
        *) printf 'refusing to remove unexpected benchmark path\n' >&2; exit 1 ;;
    esac
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$workdir/migrations/versioned"
cat > "$workdir/dbm.toml" <<'EOF'
version = 1
namespace = "benchmark"
source = ["migrations/versioned"]

[target]
driver = "sqlite"
database = "dbm.sqlite"
environment = "test"
EOF

index=1
while [ "$index" -le "$migration_count" ]; do
    version=$(printf '20261026000000%06d' "$index")
    table=$(printf 'bench_%06d' "$index")
    cat > "$workdir/migrations/versioned/${version}_${table}.sql" <<EOF
-- dbm:dialect sqlite
-- dbm:up
CREATE TABLE ${table} (id INTEGER PRIMARY KEY);
-- dbm:down
DROP TABLE ${table};
EOF
    index=$((index + 1))
done

printf 'binary_version=%s\n' "$("$binary" --version)"
printf 'platform=%s\n' "$(uname -srm)"
printf 'workload=migrations:%s samples:%s warmups:1 mode:validate-read-only\n' "$migration_count" "$samples"

run_validate() {
    "$binary" --config "$workdir/dbm.toml" --format json validate > "$workdir/result.json"
    if ! grep -Eq '"migration_count"[[:space:]]*:[[:space:]]*'"$migration_count"'([,[:space:]]|$)' "$workdir/result.json"; then
        printf 'validate returned an unexpected migration count\n' >&2
        exit 1
    fi
    if [ -e "$workdir/dbm.sqlite" ]; then
        printf 'read-only validate unexpectedly created the SQLite database\n' >&2
        exit 1
    fi
}

run_validate
printf 'warmup=complete\n'
sample=1
while [ "$sample" -le "$samples" ]; do
    start_ns=$(date +%s%N)
    run_validate
    end_ns=$(date +%s%N)
    elapsed_ms=$(((end_ns - start_ns) / 1000000))
    printf 'sample=%s elapsed_ms=%s\n' "$sample" "$elapsed_ms"
    sample=$((sample + 1))
done
