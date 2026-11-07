@echo off
REM Build entry point: tauri-winres needs rc.exe, so prepend the Windows SDK bin dir.
REM Plain `npm run tauri build` fails without it, so always go through this script.
set "PATH=C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64;%PATH%"
cd /d "%~dp0"
call npm run tauri build -- %*
