@echo off
rem Windows launcher for fake_mpv.py: CreateProcess cannot run a .py directly.
where py >nul 2>nul
if %errorlevel%==0 (
  py -3 "%~dp0fake_mpv.py" %*
) else (
  python "%~dp0fake_mpv.py" %*
)
exit /b %errorlevel%
