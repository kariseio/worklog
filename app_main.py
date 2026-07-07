"""PyInstaller 진입점 — 단일 exe 로 업무일지 데스크톱 앱을 실행한다.

빌드:
    pyinstaller worklog.spec        (또는 build_exe.bat)
결과:
    dist/worklog.exe                (더블클릭 실행)
"""

from __future__ import annotations

import os
import sys

# --windowed(콘솔 없음) 빌드에서는 sys.stdout/stderr 가 None 이 될 수 있어
# 로깅·print 가 예외를 던진다. 널 스트림으로 대체해 방지한다.
if getattr(sys, "frozen", False):
    if sys.stdout is None:
        sys.stdout = open(os.devnull, "w")  # noqa: SIM115
    if sys.stderr is None:
        sys.stderr = open(os.devnull, "w")  # noqa: SIM115

from worklog.webapp.launcher import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
