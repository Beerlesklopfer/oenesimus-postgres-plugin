#!/usr/bin/env bash
# Benchmark of the ovfs functions on the catalog of generate.sql.
#
# Runs on the host and drives the development container. Prints the test
# environment and one Markdown table with the median time and the peak
# resident memory (VmHWM) of the PostgreSQL backend per scenario.
#
#   ovfs/bench/run.sh --setup            create database ovfs_bench and load the data
#   ovfs/bench/run.sh                    warm runs: 1 warm-up + RUNS measured runs
#   ovfs/bench/run.sh --cold             one run per scenario after restarting
#                                        PostgreSQL and dropping the page cache (sudo)
#
# Environment: CONTAINER (postgres-dev), DB (ovfs_bench), RUNS (5)
set -Eeuo pipefail
export LC_ALL=C

CONTAINER=${CONTAINER:-postgres-dev}
DB=${DB:-ovfs_bench}
RUNS=${RUNS:-5}
HERE=$(cd "$(dirname "$0")" && pwd)
MODE=warm
SETUP=false
for arg in "$@"; do
    case $arg in
        --setup) SETUP=true ;;
        --cold) MODE=cold ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

psql_c() { podman exec -i -u postgres "$CONTAINER" psql -X -q -v ON_ERROR_STOP=1 "$@"; }

if $SETUP; then
    psql_c -d postgres -c "DROP DATABASE IF EXISTS $DB" \
        -c "CREATE DATABASE $DB ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
    # The Bareos DDL wraps one block in begin/commit; psql runs it as is
    psql_c -d "$DB" < "$HERE/../tests/fixtures/bareos-25.1.2-pre-postgresql.sql" > /dev/null
    psql_c -d "$DB" < "$HERE/generate.sql"
    psql_c -d "$DB" -c "CREATE EXTENSION ovfs"
    psql_c -d "$DB" -c "VACUUM (ANALYZE)"
fi

# --- test environment ---------------------------------------------------------
volume_dir=$(podman inspect "$CONTAINER" --format \
    '{{range .Mounts}}{{if eq .Destination "/var/lib/postgresql"}}{{.Source}}{{end}}{{end}}')
fs_source=$(findmnt -n -o SOURCE -T "$volume_dir" | sed 's/\[.*//')
disk=$(lsblk -n -s -o NAME,TYPE "$fs_source" | awk '$2 == "disk" {gsub(/[^a-z0-9]/, "", $1); print $1}' | head -1)
link_speed=unknown
if [[ -e /sys/block/$disk/device ]]; then
    ata=$(readlink -f "/sys/block/$disk/device" | grep -o 'ata[0-9]*' | head -1 || true)
    if [[ -n $ata ]]; then
        link_speed=$(cat "/sys/class/ata_link/link${ata#ata}/sata_spd" 2>/dev/null || echo unknown)
    fi
fi
ram_type=$(sudo -n dmidecode -t memory 2>/dev/null \
    | awk -F': ' '/^\s+Type: / && $2 != "Unknown" {t=$2} /^\s+Configured Memory Speed: / && $2 != "Unknown" {s=$2} /^\s+Size: [0-9]/ {n++} END {if (t) printf "%s, %s, %d modules", t, s, n}' || true)

echo "## Test environment ($MODE, $(date -u +%Y-%m-%dT%H:%MZ))"
echo
echo "| | |"
echo "|---|---|"
echo "| CPU | $(lscpu | sed -n 's/^Model name: *//p'), $(nproc) threads |"
echo "| RAM | $(awk '/MemTotal/ {printf "%.1f GiB", $2 / 1048576}' /proc/meminfo)${ram_type:+, $ram_type}${ram_type:- (type: needs sudo dmidecode)} |"
echo "| DB volume | \`$volume_dir\` |"
echo "| file system | $(findmnt -n -o FSTYPE,OPTIONS -T "$volume_dir" | sed 's/ /, /') |"
echo "| file attributes | \`$(lsattr -d "$volume_dir" 2>/dev/null | cut -d' ' -f1)\` (c = btrfs compression) |"
echo "| disk | $(lsblk -d -n -o MODEL,SIZE "/dev/$disk" | xargs), bus $(lsblk -d -n -o TRAN "/dev/$disk" | xargs) ($link_speed), $( [[ $(cat /sys/block/$disk/queue/rotational) == 1 ]] && echo 'rotating (HDD)' || echo 'solid state') |"
echo "| I/O scheduler | $(grep -o '\[[^]]*\]' /sys/block/$disk/queue/scheduler | tr -d '[]') |"
echo "| kernel | $(uname -r) |"
echo "| extension library | $(podman exec "$CONTAINER" bash -c 'f=$(pg_config --pkglibdir)/ovfs.so; printf "%s, %s bytes, built %s" "$f" "$(stat -c %s "$f")" "$(date -u -r "$f" +%Y-%m-%dT%H:%MZ)"') (release build ~1 MB; \`cargo pgrx test\` installs a much larger debug build) |"
echo "| container | podman $(podman --version | awk '{print $3}'), memory limit $(podman inspect "$CONTAINER" --format '{{.HostConfig.Memory}}' | sed 's/^0$/none/') |"
psql_c -d "$DB" -At -F ' | ' -c "
    SELECT '| PostgreSQL | ' || version() || ' |'
    UNION ALL SELECT '| settings | ' || string_agg(name || ' = ' || current_setting(name), ', ' ORDER BY name) || ' |'
        FROM unnest(ARRAY['shared_buffers', 'work_mem', 'effective_cache_size', 'max_parallel_workers_per_gather', 'jit', 'random_page_cost']) name
    UNION ALL SELECT '| database | ' || pg_size_pretty(pg_database_size(current_database())) || ', ' ||
        (SELECT datcollate || ' / ' || pg_encoding_to_char(encoding) FROM pg_database WHERE datname = current_database()) || ' |'
    UNION ALL SELECT '| catalog | ' || (SELECT count(*) FROM file) || ' File rows, ' || (SELECT count(*) FROM path) || ' paths |'"
echo

# --- scenarios ----------------------------------------------------------------
CHAIN="ARRAY[1,2]"
SCENARIOS=(
    "tree depth 1 from /|SELECT length(ovfs.tree($CHAIN, '/', 1)::text)"
    "tree depth 2 from /|SELECT length(ovfs.tree($CHAIN, '/', 2)::text)"
    "tree depth 3 from /|SELECT length(ovfs.tree($CHAIN, '/', 3)::text)"
    "tree depth 1 from /data/d7/|SELECT length(ovfs.tree($CHAIN, '/data/d7/', 1)::text)"
    "tree everything from /|SELECT length(ovfs.tree($CHAIN, '/', 1000, 2000000)::text)"
    "tree_latest depth 2 from /|SELECT length(ovfs.tree_latest('fd1', '/', 2)::text)"
    "ls /|SELECT count(*) FROM ovfs.ls($CHAIN, '/')"
    "ls /data/ (500 dirs)|SELECT count(*) FROM ovfs.ls($CHAIN, '/data/')"
    "ls /data/d7/s3/ (23 files)|SELECT count(*) FROM ovfs.ls($CHAIN, '/data/d7/s3/')"
    "jobids_latest|SELECT array_length(ovfs.jobids_latest('fd1'), 1)"
    "versions of one file|SELECT count(*) FROM ovfs.versions('fd1', '/data/d7/s3/', 'file-1.dat')"
)

# One query in the container; prints "<ms> <peak kB> <result>".
measure() {
    podman exec -i -u postgres -e SQL="$1" -e DB="$DB" "$CONTAINER" bash -s <<'INNER'
app="ovfs-bench-$$"
out=$(mktemp)
# Keep the backend alive briefly after the query: VmHWM is a high-water mark
# that stays until the process exits, so short queries are not missed.
PGAPPNAME=$app psql -X -At -d "$DB" -c '\timing on' -c "$SQL" -c '\timing off' -c 'SELECT pg_sleep(0.3)' > "$out" 2>&1 &
client=$!
pid=
while [[ -z $pid ]] && kill -0 $client 2>/dev/null; do
    pid=$(psql -X -At -d "$DB" -c "SELECT pid FROM pg_stat_activity WHERE application_name = '$app'")
done
peak=0
anon=0
while [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; do
    read -r hwm cur < <(awk '/VmHWM/ {h=$2} /RssAnon/ {a=$2} END {print h + 0, a + 0}' "/proc/$pid/status" 2>/dev/null) || break
    (( hwm > peak )) && peak=$hwm
    (( cur > anon )) && anon=$cur
    sleep 0.02
done
wait $client
ms=$(sed -n 's/^Time: \([0-9.]*\) ms.*/\1/p' "$out" | head -1)
result=$(grep -v -e '^Time:' -e '^Timing is' "$out" | head -1)
echo "$ms $peak $anon $result"
rm -f "$out"
INNER
}

median() { sort -n | awk '{a[NR] = $1} END {print (NR % 2) ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2}'; }

cold_start() {
    podman restart "$CONTAINER" > /dev/null
    until podman exec -u postgres "$CONTAINER" pg_isready -q; do sleep 1; done
    sync
    echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
}

echo "| Scenario | Result | Time (median of $([[ $MODE == warm ]] && echo "$RUNS" || echo 1)) | Peak RSS (VmHWM) | Peak private (RssAnon) |"
echo "|---|---|---|---|---|"
for scenario in "${SCENARIOS[@]}"; do
    name=${scenario%%|*}
    sql=${scenario#*|}
    times=()
    peaks=()
    anons=()
    if [[ $MODE == cold ]]; then
        cold_start
        read -r ms peak anon result < <(measure "$sql")
        times=("$ms"); peaks=("$peak"); anons=("$anon")
    else
        measure "$sql" > /dev/null
        for _ in $(seq "$RUNS"); do
            read -r ms peak anon result < <(measure "$sql")
            times+=("$ms"); peaks+=("$peak"); anons+=("$anon")
        done
    fi
    t=$(printf '%s\n' "${times[@]}" | median)
    p=$(printf '%s\n' "${peaks[@]}" | sort -n | tail -1)
    a=$(printf '%s\n' "${anons[@]}" | sort -n | tail -1)
    printf '| %s | %s | %.1f ms | %d MB | %d MB |\n' "$name" "$result" "$t" $((p / 1024)) $((a / 1024))
done
