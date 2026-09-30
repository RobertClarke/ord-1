#!/usr/bin/env bash
# Samples a process's resource use into a CSV, every --interval seconds.
#
#   tools/validate/resource-watch.sh --pid=PID [--data-dir=DIR] [--interval=10] [--out=FILE] [--duration=S]
#   tools/validate/resource-watch.sh --pgrep='ord.*server' ...   # re-resolve each sample (follows restarts)
#   tools/validate/resource-watch.sh --summary FILE              # peaks and averages of a CSV
#
# Columns: timestamp (UTC ISO), epoch, pid, rss_kb, hwm_kb (VmHWM, peak RSS
# of that pid), cpu_pct (all cores, so up to 100 x cores), threads,
# read_bytes / write_bytes (cumulative storage I/O of the pid, /proc/PID/io),
# read_mb_s / write_mb_s (rates since the previous sample), disk_bytes
# (du -sb of the data dir), mem_available_kb, mem_free_kb, load1, rchar /
# wchar (cumulative bytes through read/write syscalls, sockets included:
# the bitcoind RPC traffic shows here, not in read_bytes).
# Without --out the CSV goes to stdout.
set -euo pipefail

usage() {
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-2}"
}

pid=""
pattern=""
data_dir=""
interval=10
out=""
duration=0
summary=""

while [ $# -gt 0 ]; do
  case "$1" in
    --pid=*) pid="${1#*=}" ;;
    --pgrep=*) pattern="${1#*=}" ;;
    --data-dir=*) data_dir="${1#*=}" ;;
    --interval=*) interval="${1#*=}" ;;
    --out=*) out="${1#*=}" ;;
    --duration=*) duration="${1#*=}" ;;
    --summary=*) summary="${1#*=}" ;;
    --summary) shift; summary="${1:-}" ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
  shift
done

summarize() {
  local file="$1"
  [ -r "$file" ] || { echo "cannot read $file" >&2; exit 2; }
  awk -F, '
    NR == 1 { for (i = 1; i <= NF; i++) col[$i] = i; next }
    $col["pid"] == "" { next }
    {
      n++
      e = $col["epoch"] + 0
      if (n == 1) first = e
      last = e
      rss = $col["rss_kb"] + 0; hwm = $col["hwm_kb"] + 0; cpu = $col["cpu_pct"] + 0
      thr = $col["threads"] + 0; disk = $col["disk_bytes"] + 0; avail = $col["mem_available_kb"] + 0
      rmb = $col["read_mb_s"] + 0; wmb = $col["write_mb_s"] + 0
      if (rss > max_rss) max_rss = rss
      if (hwm > max_hwm) max_hwm = hwm
      if (cpu > max_cpu) max_cpu = cpu
      if (thr > max_thr) max_thr = thr
      if (disk > max_disk) max_disk = disk
      if (n == 1 || avail < min_avail) min_avail = avail
      if (rmb > max_rmb) max_rmb = rmb
      if (wmb > max_wmb) max_wmb = wmb
      sum_rss += rss
      # Rates need two samples of the same pid; the first has none.
      if ($col["cpu_pct"] != "") { rated++; sum_cpu += cpu; sum_rmb += rmb; sum_wmb += wmb }
      p = $col["pid"]
      if (!(p in rb0)) { rb0[p] = $col["read_bytes"] + 0; wb0[p] = $col["write_bytes"] + 0; rc0[p] = $col["rchar"] + 0; wc0[p] = $col["wchar"] + 0; pids++ }
      rb1[p] = $col["read_bytes"] + 0; wb1[p] = $col["write_bytes"] + 0; rc1[p] = $col["rchar"] + 0; wc1[p] = $col["wchar"] + 0
      if (n == 1) disk0 = disk
      disk1 = disk
    }
    END {
      if (n == 0) { print "no samples"; exit 1 }
      for (p in rb0) { rtot += rb1[p] - rb0[p]; wtot += wb1[p] - wb0[p]; rctot += rc1[p] - rc0[p]; wctot += wc1[p] - wc0[p] }
      gib = 1024 * 1024
      printf "samples            %d over %.1f h (%d pid%s)\n", n, (last - first) / 3600, pids, pids == 1 ? "" : "s"
      printf "rss                peak %.2f GiB, avg %.2f GiB (VmHWM peak %.2f GiB)\n", max_rss / gib, sum_rss / n / gib, max_hwm / gib
      if (!rated) rated = 1
      printf "cpu                peak %.0f%%, avg %.0f%% (100%% = one core)\n", max_cpu, sum_cpu / rated
      printf "threads            peak %d\n", max_thr
      printf "storage read       %.2f GiB total, peak %.1f MB/s, avg %.1f MB/s\n", rtot / 1024 / gib, max_rmb, sum_rmb / rated
      printf "storage write      %.2f GiB total, peak %.1f MB/s, avg %.1f MB/s\n", wtot / 1024 / gib, max_wmb, sum_wmb / rated
      printf "syscall read/write %.2f / %.2f GiB (sockets included)\n", rctot / 1024 / gib, wctot / 1024 / gib
      printf "data dir           peak %.2f GiB (first %.2f GiB, last %.2f GiB)\n", max_disk / 1024 / gib, disk0 / 1024 / gib, disk1 / 1024 / gib
      printf "memory available   min %.2f GiB\n", min_avail / gib
    }' "$file"
}

if [ -n "$summary" ]; then
  summarize "$summary"
  exit $?
fi

[ -n "$pid" ] || [ -n "$pattern" ] || usage
case "$interval" in ''|*[!0-9.]*) echo "--interval must be a number" >&2; exit 2 ;; esac

clk_tck=$(getconf CLK_TCK)
header="timestamp,epoch,pid,rss_kb,hwm_kb,cpu_pct,threads,read_bytes,write_bytes,read_mb_s,write_mb_s,disk_bytes,mem_available_kb,mem_free_kb,load1,rchar,wchar"
if [ -n "$out" ]; then
  [ -s "$out" ] || echo "$header" > "$out"
else
  echo "$header"
fi

emit() {
  if [ -n "$out" ]; then echo "$1" >> "$out"; else echo "$1"; fi
}

resolve_pid() {
  if [ -n "$pattern" ]; then
    # Oldest matching process, not counting this watcher or whatever shell
    # launched it (their command lines contain the pattern too).
    local p best="" best_start=""
    for p in $(pgrep -f -- "$pattern" || true); do
      [ "$p" = "$$" ] && continue
      { tr '\0' ' ' < "/proc/$p/cmdline"; } 2>/dev/null | grep -q 'resource-watch' && continue
      local start
      start=$(awk '{print $22}' "/proc/$p/stat" 2>/dev/null) || continue
      if [ -z "$best" ] || [ "$start" -lt "$best_start" ]; then
        best="$p"
        best_start="$start"
      fi
    done
    echo "$best"
  else
    echo "$pid"
  fi
}

prev_pid=""
prev_ticks=0
prev_time=0
prev_rb=0
prev_wb=0
started=$(date +%s)

while :; do
  now=$(date +%s.%N)
  cur=$(resolve_pid)
  mem_avail=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
  mem_free=$(awk '/^MemFree:/ {print $2}' /proc/meminfo)
  load1=$(cut -d' ' -f1 /proc/loadavg)
  disk=""
  if [ -n "$data_dir" ] && [ -e "$data_dir" ]; then
    disk=$(du -sb "$data_dir" 2>/dev/null | cut -f1 || true)
  fi
  if [ -n "$cur" ] && [ -r "/proc/$cur/status" ]; then
    rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$cur/status" 2>/dev/null || true)
    hwm=$(awk '/^VmHWM:/ {print $2}' "/proc/$cur/status" 2>/dev/null || true)
    threads=$(awk '/^Threads:/ {print $2}' "/proc/$cur/status" 2>/dev/null || true)
    # utime + stime (fields 14 and 15; the name in field 2 may contain spaces).
    ticks=$(sed 's/^.*) //' "/proc/$cur/stat" 2>/dev/null | awk '{print $12 + $13}' || echo 0)
    rb=$(awk '/^read_bytes:/ {print $2}' "/proc/$cur/io" 2>/dev/null || echo "")
    wb=$(awk '/^write_bytes:/ {print $2}' "/proc/$cur/io" 2>/dev/null || echo "")
    rchar=$(awk '/^rchar:/ {print $2}' "/proc/$cur/io" 2>/dev/null || echo "")
    wchar=$(awk '/^wchar:/ {print $2}' "/proc/$cur/io" 2>/dev/null || echo "")
    cpu=""
    rmb=""
    wmb=""
    if [ "$cur" = "$prev_pid" ]; then
      read -r cpu rmb wmb < <(awk -v t="$now" -v pt="$prev_time" -v k="$ticks" -v pk="$prev_ticks" -v hz="$clk_tck" \
        -v rb="${rb:-0}" -v prb="$prev_rb" -v wb="${wb:-0}" -v pwb="$prev_wb" \
        'BEGIN { dt = t - pt; if (dt <= 0) dt = 1; printf "%.1f %.2f %.2f\n", (k - pk) / hz / dt * 100, (rb - prb) / dt / 1e6, (wb - pwb) / dt / 1e6 }')
    fi
    emit "$(date -u +%Y-%m-%dT%H:%M:%SZ),${now%.*},$cur,${rss:-},${hwm:-},${cpu},${threads:-},${rb},${wb},${rmb},${wmb},${disk},${mem_avail},${mem_free},${load1},${rchar},${wchar}"
    prev_pid="$cur"
    prev_ticks="${ticks:-0}"
    prev_time="$now"
    prev_rb="${rb:-0}"
    prev_wb="${wb:-0}"
  else
    emit "$(date -u +%Y-%m-%dT%H:%M:%SZ),${now%.*},,,,,,,,,,${disk},${mem_avail},${mem_free},${load1},,"
    if [ -z "$pattern" ]; then
      echo "resource-watch: process $pid is gone" >&2
      exit 0
    fi
    prev_pid=""
  fi
  if [ "$duration" != 0 ] && [ $(( $(date +%s) - started )) -ge "${duration%.*}" ]; then
    exit 0
  fi
  sleep "$interval"
done
