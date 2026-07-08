"""자동 업데이트 — GitHub Releases 에서 최신 버전을 확인하고 exe 를 통째로 교체한다.

frozen exe(PyInstaller onefile) 로 실행 중일 때만 실제 교체가 동작한다.
소스/pip 실행에서는 확인만 하고 교체는 하지 않는다(exe 가 아니므로).

교체 방식(Windows 는 실행 중 exe 를 못 덮어씀):
  1. 새 exe 를 `<exe>.new` 로 다운로드(staging)
  2. UI 에서 사용자가 '확인'하면 헬퍼 배치를 띄우고 앱을 닫는다
  3. 배치가 현재 exe 잠금이 풀릴 때까지 기다렸다가 `.new` → `<exe>` 로 move(교체)
  4. 사용자가 앱을 다시 열면 새 버전이 뜬다
(yt-dlp / minio-selfupdate 등이 쓰는 표준 rename-swap 패턴)

자동 재실행(auto-relaunch)은 하지 않는다 — onefile 은 실행마다 python313.dll 을
%TEMP%\\_MEIxxxxxx 에 다시 푸는데, 방금 교체된 exe 를 백신이 스캔하는 창과 겹치면
추출이 깨져 'Failed to load Python DLL(error 126)' 이 뜬다. 이 레이스는 (자가추출
onefile 인 한) settle delay 로도 확실히 못 피한다 — 3초 지연으로도 실패를 확인했다.
그래서 '교체는 확실히, 재실행은 사용자가 직접'으로 두고, 교체 직전 UI 가 안내 알림을
띄워 갑작스런 종료를 막는다. (근본 해결은 --onedir 로 자가추출 자체를 없애는 것 — 별도 과제)
"""

from __future__ import annotations

import logging
import os
import shutil
import subprocess
import sys
import tempfile
import threading

from . import __version__

log = logging.getLogger("worklog")

REPO = "kariseio/worklog"
API_LATEST = f"https://api.github.com/repos/{REPO}/releases/latest"

# 배치 실행 플래그: CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP.
# CREATE_NO_WINDOW 는 콘솔을 '숨긴 채' 생성해, cmd 가 내부에서 돌리는 ping/move 같은
# 콘솔 프로그램이 그 숨겨진 콘솔을 물려받아 창이 뜨지 않는다. (DETACHED_PROCESS 는 콘솔을
# 아예 없애 자식 콘솔앱이 '새 콘솔창'을 띄우는 문제가 있어 안 쓴다.)
# 이 플래그로 띄운 프로세스는 부모(앱) 종료 뒤에도 살아남아 교체(swap)를 끝까지 수행한다.
_UPDATER_FLAGS = 0x08000000 | 0x00000200 if os.name == "nt" else 0

# tasklist/PID 대신 'exe 잠금이 풀릴 때까지 move 재시도' — 콘솔 없이도 안정적.
# 현재 exe 가 실행 중이면 move(=덮어쓰기)가 실패해 .new 가 남고, 프로세스가 죽으면 성공한다.
# 경로는 배치 본문에 넣지 않고 환경변수(WL_NEW/WL_EXE)로 전달 — 한글 경로가 배치 파일
# 인코딩(cmd 는 OEM 코드페이지로 읽음)에 깨지는 것을 방지. 본문은 순수 ASCII 라 안전.
# 흐름: move 먼저 시도(즉시) → 잠겨서 실패하면 ~1초 대기 후 재시도 → 교체 성공하면 종료.
# 재실행은 하지 않는다(모듈 docstring 참고) — 사용자가 앱을 직접 다시 연다.
# ping 127.0.0.1 은 콘솔 없이 쓰는 표준 sleep 트릭(-n N = 약 N-1 초).
_UPDATER_BAT = """@echo off
for /l %%i in (1,1,40) do (
  move /y "%WL_NEW%" "%WL_EXE%" >nul 2>&1
  if not exist "%WL_NEW%" goto :done
  ping -n 2 127.0.0.1 >nul
)
:done
del "%~f0"
"""


def is_frozen() -> bool:
    """PyInstaller 등으로 묶인 exe 로 실행 중인지."""
    return bool(getattr(sys, "frozen", False))


def _parse(v: str) -> tuple[int, ...]:
    """'v0.1.3' → (0, 1, 3). 숫자만 관대하게 파싱."""
    out: list[int] = []
    for part in str(v).lstrip("vV").split("."):
        num = ""
        for ch in part:
            if ch.isdigit():
                num += ch
            else:
                break
        out.append(int(num) if num else 0)
    return tuple(out)


def is_newer(latest: str, current: str) -> bool:
    return _parse(latest) > _parse(current)


def _pick_exe_asset(assets: list[dict]) -> str | None:
    """릴리스 에셋 중 exe 다운로드 URL. worklog.exe 우선, 없으면 첫 .exe."""
    if not assets:
        return None
    exact = next((a for a in assets if (a.get("name") or "").lower() == "worklog.exe"), None)
    anyexe = exact or next((a for a in assets if (a.get("name") or "").lower().endswith(".exe")), None)
    return (anyexe or {}).get("browser_download_url")


def check(timeout: float = 6.0) -> dict:
    """최신 릴리스를 확인해 결과 dict 를 반환(네트워크 실패는 error 로만 담고 예외 안 냄)."""
    res = {
        "current": __version__, "latest": None, "update_available": False,
        "download_url": None, "notes": "", "frozen": is_frozen(), "error": None,
    }
    try:
        import requests

        r = requests.get(API_LATEST, timeout=timeout,
                         headers={"Accept": "application/vnd.github+json"})
        if r.status_code != 200:
            res["error"] = f"HTTP {r.status_code}"
            return res
        data = r.json()
        tag = (data.get("tag_name") or "").lstrip("vV")
        res["latest"] = tag or None
        res["notes"] = data.get("body") or ""
        res["download_url"] = _pick_exe_asset(data.get("assets") or [])
        res["update_available"] = bool(tag and is_newer(tag, __version__) and res["download_url"])
    except Exception as e:  # noqa: BLE001
        res["error"] = str(e)
    return res


def download_and_stage(download_url: str, timeout: float = 180.0) -> str:
    """새 exe 를 현재 실행파일 옆 `<exe>.new` 로 내려받고 그 경로를 반환."""
    import requests

    exe = os.path.abspath(sys.executable)
    new = exe + ".new"
    with requests.get(download_url, stream=True, timeout=timeout) as r:
        r.raise_for_status()
        with open(new, "wb") as f:
            for chunk in r.iter_content(65536):
                if chunk:
                    f.write(chunk)
    if os.path.getsize(new) < 1_000_000:   # 정상 exe 는 수십 MB — 너무 작으면 이상
        os.remove(new)
        raise RuntimeError("다운로드 파일이 비정상적으로 작습니다.")
    return new


def cleanup_stale_extractions() -> int:
    r"""이전 실행에서 남은(누출된) onefile 추출 폴더(%TEMP%\_MEIxxxxxx)를 청소한다.

    자동 업데이트가 os._exit 로 즉시 종료하거나 강제 종료되면 PyInstaller 의 임시폴더
    정리(atexit)가 건너뛰어져 폴더가 계속 쌓인다(각 ~50MB). frozen(onefile) 실행일 때만
    동작하며, 다음은 건드리지 않아 안전하다:
      - 현재 실행 중인 폴더(sys._MEIPASS)
      - '사용 중'이라 rmtree 가 실패하는 폴더(다른 실행이 DLL 을 잠금) → skip
      - worklog 서명(worklog/webapp/static)이 없는 폴더(다른 onefile 앱) → skip
    """
    if os.name != "nt" or not is_frozen():
        return 0
    removed = 0
    try:
        mypass = os.path.normcase(os.path.abspath(getattr(sys, "_MEIPASS", "") or ""))
        tmp = tempfile.gettempdir()
        for name in os.listdir(tmp):
            if not name.startswith("_MEI"):
                continue
            d = os.path.join(tmp, name)
            if os.path.normcase(os.path.abspath(d)) == mypass:
                continue
            if not os.path.isdir(os.path.join(d, "worklog", "webapp", "static")):
                continue  # worklog 것이 아니면 skip(타 앱 보호)
            try:
                shutil.rmtree(d)   # 사용 중(잠김)이면 OSError → skip
                removed += 1
            except OSError:
                pass
    except OSError:
        pass
    if removed:
        log.info("이전 업데이트 임시폴더 %d개 정리", removed)
    return removed


def schedule_apply_and_restart(new_exe: str, delay: float = 0.8) -> None:
    """헬퍼 배치(잠금 풀릴 때까지 대기 → 교체)를 띄우고, 잠시(delay) 뒤 이 프로세스를 종료 예약.

    재실행은 하지 않는다(모듈 docstring 참고) — 교체만 하고, 사용자가 앱을 다시 연다.
    delay 는 finalize 응답이 브라우저로 나갈 시간만 확보하면 되므로 짧게 잡는다.
    """
    exe = os.path.abspath(sys.executable)
    bat = os.path.join(tempfile.gettempdir(), "worklog_update.bat")
    with open(bat, "w", encoding="ascii") as f:   # 본문 순수 ASCII (경로는 env 로 전달)
        f.write(_UPDATER_BAT)
    env = dict(os.environ, WL_NEW=os.path.abspath(new_exe), WL_EXE=exe)   # 유니코드 경로 보존
    subprocess.Popen(
        ["cmd", "/c", bat], env=env, creationflags=_UPDATER_FLAGS, close_fds=True,
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    log.info("업데이트 적용: 앱을 닫습니다 (%s → %s)", exe, new_exe)
    threading.Timer(delay, lambda: os._exit(0)).start()
