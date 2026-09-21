@echo off
setlocal enabledelayedexpansion

:: ============================================
:: Cognix Launcher - Auto-start ZAI Proxy
:: ============================================
:: This script:
:: 1. Checks if ZAI Proxy is already running
:: 2. Starts proxy in background if not running
:: 3. Waits for proxy to be healthy
:: 4. Launches Cognix
:: 5. Optionally stops proxy when Cognix exits
:: ============================================

title Cognix Launcher

:: Configuration
set "PROXY_PORT=3001"
set "PROXY_HOST=localhost"
set "COGNIX_PORT=8080"
set "HEALTH_CHECK_URL=http://%PROXY_HOST%:%PROXY_PORT%/health"
set "COGNIX_EXE=%~dp0Cognix.exe"
set "PROXY_EXE=%~dp0zai-proxy.exe"
set "CLI_EXE=%~dp0cli.exe"
set "PROXY_PID_FILE=%TEMP%\cognix-proxy.pid"
set "LOG_DIR=%~dp0logs"
set "PROXY_LOG=%LOG_DIR%\zai-proxy.log"
set "MAX_WAIT_SECONDS=30"

:: Colors for output
set "GREEN=[92m"
set "RED=[91m"
set "YELLOW=[93m"
set "BLUE=[94m"
set "RESET=[0m"

echo.
echo %BLUE%========================================%RESET%
echo %BLUE%  Cognix Launcher%RESET%
echo %BLUE%  ZAI Proxy + Editor Auto-Start%RESET%
echo %BLUE%========================================%RESET%
echo.

:: Create logs directory
if not exist "%LOG_DIR%" mkdir "%LOG_DIR%"

:: Check if Cognix exists
if not exist "%COGNIX_EXE%" (
    echo %RED%ERROR: Cognix.exe not found at:%RESET%
    echo   %COGNIX_EXE%
    echo.
    echo Please ensure this script is in the same folder as Cognix.exe
    pause
    exit /b 1
)

:: Check if proxy exists
if not exist "%PROXY_EXE%" (
    echo %YELLOW%WARNING: zai-proxy.exe not found at:%RESET%
    echo   %PROXY_EXE%
    echo.
    echo The proxy must be running separately.
    echo Starting Cognix without auto-proxy...
    echo.
    goto :start_cognix
)

:: Step 1: Check if proxy is already running
echo %BLUE%[1/4]%RESET% Checking if ZAI Proxy is already running...

:: Try health check
curl -s -o nul -w "%%{http_code}" "%HEALTH_CHECK_URL%" > nul 2>&1
if %ERRORLEVEL% equ 0 (
    for /f %%a in ('curl -s -o nul -w "%%{http_code}" "%HEALTH_CHECK_URL%"') do set "HTTP_CODE=%%a"
    if "!HTTP_CODE!"=="200" (
        echo %GREEN%  ✓ Proxy is already running (HTTP !HTTP_CODE!)%RESET%
        goto :proxy_ready
    )
)

:: Check by port
netstat -an | findstr ":%PROXY_PORT% " | findstr "LISTENING" > nul 2>&1
if %ERRORLEVEL% equ 0 (
    echo %GREEN%  ✓ Proxy port %PROXY_PORT% is already in use%RESET%
    goto :proxy_ready
)

:: Step 2: Start proxy
echo %BLUE%[2/4]%RESET% Starting ZAI Proxy...

:: Check if proxy needs database
if not exist "%~dp0tokens.sqlite" (
    echo %YELLOW%  Note: tokens.sqlite not found. Proxy may need initialization.%RESET%
)

:: Start proxy in background
start "" /B cmd /c "%PROXY_EXE% > "%PROXY_LOG%" 2>&1"

:: Save PID for cleanup
for /f "tokens=2" %%a in ('tasklist /fi "imagename eq zai-proxy.exe" /fo list ^| findstr "PID"') do (
    set "PROXY_PID=%%a"
    echo !PROXY_PID! > "%PROXY_PID_FILE%"
)

echo %GREEN%  ✓ Proxy started (PID: !PROXY_PID!)%RESET%

:: Step 3: Wait for proxy to be healthy
echo %BLUE%[3/4]%RESET% Waiting for proxy to be ready...

set /a "wait_count=0"
:wait_loop
if %wait_count% geq %MAX_WAIT_SECONDS% (
    echo %RED%  ✗ Proxy did not become ready within %MAX_WAIT_SECONDS% seconds%RESET%
    echo %YELLOW%  Continuing anyway...%RESET%
    goto :proxy_ready
)

curl -s -o nul -w "%%{http_code}" "%HEALTH_CHECK_URL%" > nul 2>&1
if %ERRORLEVEL% equ 0 (
    for /f %%a in ('curl -s -o nul -w "%%{http_code}" "%HEALTH_CHECK_URL%"') do set "HTTP_CODE=%%a"
    if "!HTTP_CODE!"=="200" (
        echo %GREEN%  ✓ Proxy is ready!%RESET%
        goto :proxy_ready
    )
)

set /a "wait_count+=1"
timeout /t 1 /nobreak > nul
goto :wait_loop

:proxy_ready
echo.

:: Step 4: Launch Cognix
:start_cognix
echo %BLUE%[4/4]%RESET% Launching Cognix...

:: Set environment for Cognix
set "GLM_API_URL=http://localhost:%COGNIX_PORT%"
set "ZAI_PROXY_URL=http://localhost:%PROXY_PORT%"

:: Launch Cognix with any arguments passed to this script
start "" "%COGNIX_EXE%" %*

echo %GREEN%  ✓ Cognix launched!%RESET%
echo.
echo %BLUE%========================================%RESET%
echo %BLUE%  Cognix is running%RESET%
echo %BLUE%  Proxy: http://localhost:%PROXY_PORT%%RESET%
echo %BLUE%========================================%RESET%
echo.
echo Press any key to stop proxy and exit...
pause > nul

:: Cleanup: Stop proxy
echo.
echo %YELLOW%Stopping proxy...%RESET%

if exist "%PROXY_PID_FILE%" (
    set /p PROXY_PID=<"%PROXY_PID_FILE%"
    taskkill /PID !PROXY_PID! /F > nul 2>&1
    del "%PROXY_PID_FILE%" > nul 2>&1
)

:: Also kill any remaining proxy processes
taskkill /IM "zai-proxy.exe" /F > nul 2>&1

echo %GREEN%✓ Proxy stopped%RESET%
echo.

endlocal
exit /b 0
