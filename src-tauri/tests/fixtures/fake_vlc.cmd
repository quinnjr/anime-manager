@echo off
rem Windows launcher for fake_vlc.py: CreateProcess cannot run a .py directly.
where py >nul 2>nul
if %errorlevel%==0 (
  py -3 "%~dp0fake_vlc.py" %*
) else (
  python "%~dp0fake_vlc.py" %*
)
exit /b %errorlevel%
