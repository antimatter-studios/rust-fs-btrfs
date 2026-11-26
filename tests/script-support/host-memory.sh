# shellcheck shell=bash
# host-memory.sh — sourced by the guards that drive the harness's stub engine.
#
# The harness refuses to boot a guest the host has no room for: the guest's
# memory plus 2048 MiB. A stub engine boots nothing, so a guard's verdict must
# not depend on the machine running it, and each guard answers the harness's
# question itself (#780, #813). The admission is the real one; only what it
# reads is the guard's.
#
# give_host_memory SANDBOX MIB — from here on, the host has MIB MiB available.
give_host_memory() {
    local sandbox="$1" mib="$2"
    local bin="$sandbox/memory-bin"
    mkdir -p "$bin"
    # Linux: the harness reads this file in place of /proc/meminfo.
    export FLTH_TEST_MEMINFO="$sandbox/meminfo"
    printf 'MemTotal: %s kB\nMemAvailable: %s kB\n' "$((mib * 1024))" "$((mib * 1024))" > "$FLTH_TEST_MEMINFO"
    # macOS: it reads vm_stat. 64 pages of 16 KiB make a MiB.
    cat > "$bin/vm_stat" <<STUB
#!/bin/sh
printf '%s\n' 'Mach Virtual Memory Statistics: (page size of 16384 bytes)' 'Pages free: $((mib * 64)).' 'Pages inactive: 0.' 'Pages speculative: 0.'
STUB
    chmod +x "$bin/vm_stat"
    case ":$PATH:" in
        *":$bin:"*) ;;
        *) export PATH="$bin:$PATH" ;;
    esac
}
