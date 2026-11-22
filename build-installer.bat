@echo off
rem Builds the release executable and compiles the OmniPHP installer.
rem
rem   build-installer.bat          complete installer (app + runtimes in bin\)
rem   build-installer.bat light    app only, no runtimes
setlocal
set "ROOT=%~dp0"

set "ISCC=%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe"
if not exist "%ISCC%" set "ISCC=%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe"
if not exist "%ISCC%" set "ISCC=%ProgramFiles%\Inno Setup 6\ISCC.exe"
if not exist "%ISCC%" (
    echo Inno Setup 6 was not found. Install it from https://jrsoftware.org/isdl.php
    exit /b 1
)

set "FLAGS="
if /i "%~1"=="light" set "FLAGS=/DBundleRuntimes=0"

echo === Building omniphp.exe (release) ===
pushd "%ROOT%omniphp"
cargo build --release
if errorlevel 1 (
    popd
    echo Build failed. If omniphp.exe is running from target\release, quit it and try again.
    exit /b 1
)
popd

echo === Compiling the installer ===
"%ISCC%" /Q %FLAGS% "%ROOT%installer\omniphp.iss"
if errorlevel 1 (
    echo The installer could not be compiled.
    exit /b 1
)

echo.
echo Done:
dir /b "%ROOT%dist\OmniPHP-Setup-*.exe"
echo in %ROOT%dist
endlocal
