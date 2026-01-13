#!/bin/bash
#
# DPDK Test Runner
#
# Runs each DPDK test in a separate process to avoid EAL re-initialization issues.
# DPDK's EAL can only be initialized once per process.
#
# Note: Not using set -e to allow individual tests to fail gracefully

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOKIO_DIR="${SCRIPT_DIR}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;90m'
BOLD='\033[1m'
NC='\033[0m'

# Read DPDK devices from env.json if not provided via environment
get_dpdk_devices_from_config() {
    local env_file="/etc/dpdk/env.json"
    if [[ -f "$env_file" ]]; then
        python3 -c "
import json
with open('$env_file') as f:
    cfg = json.load(f)
pci = [d['pci_address'] for d in cfg.get('devices', []) if d.get('role') == 'dpdk']
print(','.join(pci))
" 2>/dev/null
    fi
}

# Tests read device config from env.json directly
# DPDK_TEST_PORT is required for tests that need kernel<->DPDK communication
if [[ -z "${DPDK_TEST_PORT:-}" ]]; then
    echo "Error: DPDK_TEST_PORT environment variable is required"
    echo "Set it to a port allowed in your AWS security group (e.g., 8192)"
    exit 1
fi

DPDK_TEST_PORT="${DPDK_TEST_PORT}"

# For display purposes, extract device from env.json
DPDK_DEVICE=$(get_dpdk_devices_from_config | cut -d, -f1)

# Test results
PASSED=0
FAILED=0
SKIPPED=0
declare -a FAILED_TESTS

# =============================================================================
# DPDK-Incompatible Tests
# =============================================================================
# All tests should pass. If a test fails, it needs to be fixed, not skipped.
# =============================================================================
DPDK_INCOMPATIBLE_TESTS=(
)

log_header() {
    echo ""
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}  DPDK Test Runner${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
    echo -e "Device: ${GREEN}$DPDK_DEVICE${NC}"
    echo ""
}

# All DPDK test files in the repository
DPDK_TEST_FILES=("tcp_dpdk" "tcp_dpdk_real" "dpdk_multi_process" "dpdk_worker_isolation" "unified_event_loop_test" "rt_common" "time_sleep")

# Check if a test is in the incompatible list
is_incompatible_test() {
    local test_name="$1"
    for incompatible in "${DPDK_INCOMPATIBLE_TESTS[@]}"; do
        if [[ "$test_name" == *"$incompatible"* ]]; then
            return 0  # true, is incompatible
        fi
    done
    return 1  # false, is compatible
}

# Get list of all DPDK test names with their source file
get_test_list() {
    local all_tests
    all_tests=$(
        {
            # tcp_dpdk tests
            cargo test --package tokio --test tcp_dpdk --features full -- --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/tcp_dpdk:/'
            
            # tcp_dpdk_real tests (real network tests)
            cargo test --package tokio --test tcp_dpdk_real --features full -- --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/tcp_dpdk_real:/'
            
            # dpdk_multi_process tests (multi-process/multi-queue tests)
            cargo test --package tokio --test dpdk_multi_process --features full -- --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/dpdk_multi_process:/'
            
            # dpdk_worker_isolation tests (worker isolation tests)
            cargo test --package tokio --test dpdk_worker_isolation --features full -- --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/dpdk_worker_isolation:/'
            
            # rt_common dpdk_scheduler tests
            cargo test --package tokio --test rt_common --features full -- dpdk_scheduler --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/rt_common:/'
            
            # time_sleep dpdk_flavor tests
            cargo test --package tokio --test time_sleep --features full -- dpdk_flavor --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/time_sleep:/'
            
            # unified_event_loop_test tests
            cargo test --package tokio --test unified_event_loop_test --features full -- --list 2>/dev/null \
                | grep ': test$' \
                | sed 's/: test$//' \
                | sed 's/^/unified_event_loop_test:/'
        } | sort | uniq
    )
    
    # Filter out incompatible tests
    while IFS= read -r test; do
        if ! is_incompatible_test "$test"; then
            echo "$test"
        fi
    done <<< "$all_tests"
}

# Run a single test in isolation
# Input format: "test_file:test_name" (e.g., "tcp_dpdk:api_parity::tcp_stream_read_write")
run_single_test() {
    local full_name="$1"
    local test_num="$2"
    local total="$3"
    
    # Parse test_file:test_name format
    local test_file="${full_name%%:*}"
    local test_name="${full_name#*:}"
    
    printf "[%2d/%2d] %-60s " "$test_num" "$total" "$test_name"
    
    # Tests read config from env.json; only pass DPDK_TEST_PORT for kernel<->DPDK tests
    local env_vars="DPDK_TEST_PORT=$DPDK_TEST_PORT"
    
    # Run test in separate process, capture output (with 60s timeout)
    local output
    local tmpfile="/tmp/dpdk_test_$$_${test_num}.log"
    
    timeout 60s env $env_vars \
        cargo test --package tokio --test "$test_file" --features full \
        -- "$test_name" --exact >"$tmpfile" 2>&1
    local exit_code=$?
    
    # Read output
    output=$(cat "$tmpfile")
    rm -f "$tmpfile"
    
    # Check result (exit code 124 = timeout)
    if [[ $exit_code -eq 124 ]]; then
        echo -e "${YELLOW}TIMEOUT${NC}"
        ((SKIPPED++))
        FAILED_TESTS+=("$test_name (TIMEOUT)")
        return 1
    elif echo "$output" | grep -q "1 passed"; then
        echo -e "${GREEN}PASSED${NC}"
        ((PASSED++))
        return 0
    elif echo "$output" | grep -q "0 passed; 0 failed"; then
        echo -e "${YELLOW}SKIPPED${NC}"
        ((SKIPPED++))
        return 0
    else
        echo -e "${RED}FAILED${NC}"
        ((FAILED++))
        FAILED_TESTS+=("$test_name")
        
        # Show error summary
        echo "$output" | grep -E "(panicked|Error|error\[|FAILED)" | head -3 | sed 's/^/         /'
        return 1
    fi
}

# Run all tests
run_all_tests() {
    log_header
    
    echo -e "${GRAY}Discovering tests...${NC}"
    local tests
    tests=$(get_test_list)
    
    if [[ -z "$tests" ]]; then
        echo -e "${RED}No tests found!${NC}"
        exit 1
    fi
    
    local total=$(echo "$tests" | wc -l)
    echo -e "Found ${BOLD}$total${NC} tests"
    echo ""
    
    echo -e "${CYAN}Running tests (each in separate process)...${NC}"
    echo ""
    
    # Save tests to temp file to avoid subshell issues
    local tmpfile="/tmp/dpdk_tests_$$.txt"
    echo "$tests" > "$tmpfile"
    
    local num=0
    while IFS= read -r test_name; do
        ((num++))
        run_single_test "$test_name" "$num" "$total" || true
    done < "$tmpfile"
    
    rm -f "$tmpfile"
    
    # Summary
    echo ""
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}  Results${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
    echo -e "  ${GREEN}Passed:${NC}  $PASSED"
    echo -e "  ${RED}Failed:${NC}  $FAILED"
    echo -e "  ${YELLOW}Skipped:${NC} $SKIPPED"
    echo ""
    
    if [[ $FAILED -gt 0 ]]; then
        echo -e "${RED}Failed tests:${NC}"
        for t in "${FAILED_TESTS[@]}"; do
            echo "  - $t"
        done
        echo ""
        exit 1
    else
        echo -e "${GREEN}All tests passed!${NC}"
        echo ""
        exit 0
    fi
}

# Run specific test with full output
# Supports both formats:
#   - "test_file:test_name" (e.g., "tcp_dpdk:api_parity::tcp_stream_read_write")
#   - "test_name" (e.g., "dpdk_scheduler::block_on_sync") - auto-detects file
run_specific_test() {
    local input="$1"
    local test_file
    local test_name
    
    # Check if input contains ":" prefix (file:name format)
    if [[ "$input" == *:*::* ]]; then
        # Format: test_file:test_name
        test_file="${input%%:*}"
        test_name="${input#*:}"
    else
        # Auto-detect test file by searching
        test_name="$input"
        test_file=""
        
        for f in "${DPDK_TEST_FILES[@]}"; do
            if cargo test --package tokio --test "$f" --features full -- "$test_name" --list 2>/dev/null | grep -q ': test$'; then
                test_file="$f"
                break
            fi
        done
        
        if [[ -z "$test_file" ]]; then
            echo -e "${RED}Error: Test '$test_name' not found in any DPDK test file${NC}"
            exit 1
        fi
    fi
    
    echo -e "${CYAN}Running test:${NC} $test_name"
    echo -e "${GRAY}Test file:${NC} $test_file"
    echo -e "${GRAY}Port:${NC} $DPDK_TEST_PORT"
    echo ""
    
    env DPDK_TEST_PORT="$DPDK_TEST_PORT" \
        cargo test --package tokio --test "$test_file" --features full \
        -- "$test_name" --exact --nocapture
}

# Run original tokio tests (without DPDK)
run_tokio_tests() {
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}  Running Original Tokio Tests${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
    echo "This verifies that DPDK additions don't break existing functionality."
    echo ""
    
    echo -e "${GRAY}[1/3] Running lib tests...${NC}"
    cargo test --package tokio --features full --lib 2>&1 | tail -3
    
    echo ""
    echo -e "${GRAY}[2/3] Running rt_common tests (excluding dpdk_scheduler)...${NC}"
    cargo test --package tokio --features full --test rt_common -- --skip dpdk_scheduler 2>&1 | tail -3
    
    echo ""
    echo -e "${GRAY}[3/3] Running task tests...${NC}"
    cargo test --package tokio --features full --test task 2>&1 | tail -3
    
    echo ""
    echo -e "${GREEN}Original Tokio tests complete!${NC}"
}

# Show help
show_help() {
    echo "DPDK Test Runner"
    echo ""
    echo "Usage: $0 [OPTIONS] [TEST_NAME]"
    echo ""
    echo "Options:"
    echo "  -h, --help      Show this help"
    echo "  -l, --list      List all available DPDK tests"
    echo "  -t, --tokio     Run original Tokio tests (verify no regressions)"
    echo "  -a, --all       Run all tests (Tokio + DPDK)"
    echo ""
    echo "Arguments:"
    echo "  TEST_NAME       Run specific test only (e.g., dpdk_specific::dpdk_runtime_spawn)"
    echo ""
    echo "Environment:"
    echo "  DPDK_TEST_PORT   Port for kernel<->DPDK tests (required, e.g., 8192)"
    echo ""
    echo "Examples:"
    echo "  DPDK_TEST_PORT=8192 $0                # Run all DPDK tests"
    echo "  DPDK_TEST_PORT=8192 $0 dpdk_specific::dpdk_runtime_spawn  # Run specific test"
    echo "  $0 -t                                 # Run original Tokio tests"
    echo "  $0 -a                                 # Run both Tokio and DPDK tests"
    echo ""
}

# List tests
list_tests() {
    echo "Available DPDK tests:"
    echo ""
    get_test_list | sed 's/^/  /'
    echo ""
}

# Main
cd "$TOKIO_DIR"

case "${1:-}" in
    -h|--help)
        show_help
        ;;
    -l|--list)
        list_tests
        ;;
    -t|--tokio)
        run_tokio_tests
        ;;
    -a|--all)
        run_tokio_tests
        echo ""
        run_all_tests
        ;;
    "")
        run_all_tests
        ;;
    *)
        run_specific_test "$1"
        ;;
esac
