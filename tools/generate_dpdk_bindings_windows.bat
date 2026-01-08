@echo off
REM DPDK Windows FFI Binding Generator v2

call "D:\Program Files\Visual Studio\VC\Auxiliary\Build\vcvarsall.bat" x64

set LIBCLANG_PATH=D:\Program Files\LLVM\bin
set DPDK_DIR=F:\dpdk\dpdk-23.11
set BUILD_DIR=%DPDK_DIR%\build
set OUT_FILE=F:\tokio-dpdk\tokio\src\runtime\scheduler\dpdk\ffi\bindings_windows.rs

REM Create wrapper header
echo /* Auto-generated DPDK wrapper for Windows */ > %BUILD_DIR%\dpdk_wrapper.h
echo #include ^<rte_eal.h^> >> %BUILD_DIR%\dpdk_wrapper.h
echo #include ^<rte_ethdev.h^> >> %BUILD_DIR%\dpdk_wrapper.h
echo #include ^<rte_mbuf.h^> >> %BUILD_DIR%\dpdk_wrapper.h
echo #include ^<rte_mempool.h^> >> %BUILD_DIR%\dpdk_wrapper.h
echo #include ^<rte_lcore.h^> >> %BUILD_DIR%\dpdk_wrapper.h

echo Running bindgen...

bindgen %BUILD_DIR%\dpdk_wrapper.h ^
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
    --output %OUT_FILE% ^
    -- ^
    -I %DPDK_DIR%\lib\eal\include ^
    -I %DPDK_DIR%\lib\eal\windows\include ^
    -I %DPDK_DIR%\lib\eal\x86\include ^
    -I %BUILD_DIR% ^
    -I %DPDK_DIR%\lib\ethdev ^
    -I %DPDK_DIR%\lib\mbuf ^
    -I %DPDK_DIR%\lib\mempool ^
    -I %DPDK_DIR%\lib\net ^
    -I %DPDK_DIR%\lib\kvargs ^
    -I %DPDK_DIR%\lib\log ^
    -I %DPDK_DIR%\lib\ring ^
    -I %DPDK_DIR%\lib\meter ^
    -I %DPDK_DIR%\lib\telemetry ^
    -I %DPDK_DIR%\config

if exist %OUT_FILE% (
    echo SUCCESS: Generated %OUT_FILE%
    for %%A in (%OUT_FILE%) do echo File size: %%~zA bytes
) else (
    echo FAILED: Could not generate bindings
)
