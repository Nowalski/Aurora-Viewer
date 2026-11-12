@echo off
rem Shared place test build: host for guests on other PCs.
rem Listens on port 7777 for the whole network. The invitations carry
rem ADDRESS: this PC's network address (same home network), or your
rem public IP with TCP port 7777 forwarded on the router (internet).
rem Sharing starts once you are logged in (World > Stop sharing ends it).
set ADDRESS=
if "%ADDRESS%"=="" for /f "tokens=2 delims=:" %%a in ('ipconfig ^| findstr /c:"IPv4"') do if not defined ADDRESS set ADDRESS=%%a
set ADDRESS=%ADDRESS: =%
set AURORA_INSTANCE_HOST=7777
set AURORA_INSTANCE_BIND=0.0.0.0
set AURORA_INSTANCE_ADVERTISE=%ADDRESS%:7777
echo Sharing on %ADDRESS%:7777
start "" "%~dp0aurora-viewer.exe"
