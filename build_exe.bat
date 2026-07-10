@echo off
REM ===========================================================================
REM  업무일지 단일 실행파일(.exe) 빌드
REM  결과: dist\worklog.exe  (더블클릭으로 실행)
REM  필요: uv, 그리고 앱 의존성 설치  ->  uv pip install -e ".[app]" pyinstaller
REM ===========================================================================
cd /d "%~dp0"
echo [worklog] building single-file exe ...

uv run pyinstaller --noconfirm --onefile --windowed --name worklog ^
  --add-data "worklog/webapp/static;worklog/webapp/static" ^
  --add-data "worklog/templates;worklog/templates" ^
  --collect-submodules uvicorn ^
  --collect-submodules pystray ^
  --collect-all webview ^
  --collect-all zstandard ^
  --exclude-module pytest ^
  app_main.py

REM releases 폴더에 복사: 버전별 보관(worklog-<버전>.exe) + 최신(worklog.exe)
for /f %%v in ('uv run python -c "import worklog;print(worklog.__version__)"') do set VER=%%v
if not exist releases mkdir releases
copy /y dist\worklog.exe "releases\worklog-%VER%.exe" >nul
copy /y dist\worklog.exe "releases\worklog.exe" >nul

echo.
echo [worklog] done -^> dist\worklog.exe
echo [worklog] archived -^> releases\worklog-%VER%.exe
echo [worklog] latest   -^> releases\worklog.exe
pause
