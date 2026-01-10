@echo off
REM DPDK Windows FFI Binding Generator
REM
REM Prerequisites:
REM   - Visual Studio with C++ Build Tools
REM   - LLVM/Clang installed (for bindgen)
REM   - DPDK source or installed headers
REM   - Rust bindgen CLI: cargo install bindgen-cli
REM
REM Environment Variables (must be set before running):
REM   DPDK_DIR     - Path to DPDK source/install (e.g., C:\dpdk\dpdk-23.11)
REM   LLVM_DIR     - Path to LLVM installation (e.g., C:\Program Files\LLVM)
REM   VCVARS_PATH  - Path to vcvarsall.bat (optional, auto-detected)

setlocal enabledelayedexpansion

REM Validate required environment variables
if not defined DPDK_DIR (
    echo ERROR: DPDK_DIR environment variable not set
    echo Example: set DPDK_DIR=C:\dpdk\dpdk-23.11
    exit /b 1
)

if not exist "%DPDK_DIR%" (
    echo ERROR: DPDK_DIR does not exist: %DPDK_DIR%
    exit /b 1
)

REM Set up LLVM path for bindgen
if defined LLVM_DIR (
    set LIBCLANG_PATH=%LLVM_DIR%\bin
) else (
    echo WARNING: LLVM_DIR not set, assuming LLVM is in PATH
)

REM Initialize Visual Studio environment
if defined VCVARS_PATH (
    call "%VCVARS_PATH%" x64
) else (
    REM Try common locations
    for %%V in (
        "%ProgramFiles%\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvarsall.bat"
        "%ProgramFiles%\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvarsall.bat"
        "%ProgramFiles%\Microsoft Visual Studio\2019\Community\VC\Auxiliary\Build\vcvarsall.bat"
        "%ProgramFiles(x86)%\Microsoft Visual Studio\2019\Community\VC\Auxiliary\Build\vcvarsall.bat"
    ) do (
        if exist %%V (
            call %%V x64
            goto :vcvars_done
        )
    )
    echo WARNING: Could not find vcvarsall.bat, continuing anyway...
)
:vcvars_done

REM Set paths
set BUILD_DIR=%DPDK_DIR%\build
set SCRIPT_DIR=%~dp0
set OUT_FILE=%SCRIPT_DIR%..\tokio\src\runtime\scheduler\dpdk\ffi\bindings_windows.rs

REM Create build directory if needed
if not exist "%BUILD_DIR%" mkdir "%BUILD_DIR%"

REM Create wrapper header
echo /* Auto-generated DPDK wrapper for Windows */ > "%BUILD_DIR%\dpdk_wrapper.h"
echo #include ^<rte_eal.h^> >> "%BUILD_DIR%\dpdk_wrapper.h"
echo #include ^<rte_ethdev.h^> >> "%BUILD_DIR%\dpdk_wrapper.h"
echo #include ^<rte_mbuf.h^> >> "%BUILD_DIR%\dpdk_wrapper.h"
echo #include ^<rte_mempool.h^> >> "%BUILD_DIR%\dpdk_wrapper.h"
echo #include ^<rte_lcore.h^> >> "%BUILD_DIR%\dpdk_wrapper.h"

echo.
echo === DPDK Windows Bindings Generator ===
echo DPDK_DIR: %DPDK_DIR%
echo Output:   %OUT_FILE%
echo.
echo Running bindgen...

bindgen "%BUILD_DIR%\dpdk_wrapper.h" ^
    --allowlist-function "rte_eal_init" ^
    --allowlist-function "rte_eal_cleanup" ^
    --allowlist-function "rte_eth_dev_count_avail" ^
    --allowlist-function "rte_eth_dev_configure" ^
    --allowlist-function "rte_eth_dev_start" ^
    --allowlist-function "rte_eth_dev_stop" ^
    --allowlist-function "rte_eth_dev_close" ^
    --allowlist-function "rte_eth_rx_queue_setup" ^
    --allowlist-function "rte_eth_tx_queue_setup" ^
    --allowlist-function "rte_eth_promiscuous_enable" ^
    --allowlist-function "rte_eth_dev_socket_id" ^
    --allowlist-function "rte_eth_dev_info_get" ^
    --allowlist-function "rte_eth_macaddr_get" ^
    --allowlist-function "rte_pktmbuf_pool_create" ^
    --allowlist-function "rte_mempool_free" ^
    --allowlist-function "rte_socket_id" ^
    --allowlist-function "rte_lcore_id" ^
    --allowlist-type "rte_mbuf" ^
    --allowlist-type "rte_mempool" ^
    --allowlist-type "rte_eth_conf" ^
    --allowlist-type "rte_eth_dev_info" ^
    --allowlist-type "rte_eth_rxconf" ^
    --allowlist-type "rte_eth_txconf" ^
    --allowlist-type "rte_ether_addr" ^
    --use-core ^
    --output "%OUT_FILE%" ^
    -- ^
    -I "%DPDK_DIR%\lib\eal\include" ^
    -I "%DPDK_DIR%\lib\eal\windows\include" ^
    -I "%DPDK_DIR%\lib\eal\x86\include" ^
    -I "%BUILD_DIR%" ^
    -I "%DPDK_DIR%\lib\ethdev" ^
    -I "%DPDK_DIR%\lib\mbuf" ^
    -I "%DPDK_DIR%\lib\mempool" ^
    -I "%DPDK_DIR%\lib\net" ^
    -I "%DPDK_DIR%\lib\kvargs" ^
    -I "%DPDK_DIR%\lib\log" ^
    -I "%DPDK_DIR%\lib\ring" ^
    -I "%DPDK_DIR%\lib\meter" ^
    -I "%DPDK_DIR%\lib\telemetry" ^
    -I "%DPDK_DIR%\config"

if exist "%OUT_FILE%" (
    echo.
    echo SUCCESS: Generated %OUT_FILE%
    for %%A in ("%OUT_FILE%") do echo File size: %%~zA bytes
) else (
    echo.
    echo FAILED: Could not generate bindings
    exit /b 1
)

endlocal
