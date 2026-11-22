@echo off
rem Downloads the runtimes listed in runtimes.json (Apache, PHP, MySQL, ...) into bin\.
rem
rem   fetch-runtimes.bat                      everything that is missing
rem   fetch-runtimes.bat -List                show what is installed, download nothing
rem   fetch-runtimes.bat -Only apache,mysql   just these
rem   fetch-runtimes.bat -IncludeOptional     also the optional ones (Git)
setlocal
rem Started from a PowerShell 7 prompt, Windows PowerShell would inherit a module
rem path it cannot use; cleared, it falls back to its own.
set "PSModulePath="
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\fetch-runtimes.ps1" %*
exit /b %errorlevel%
