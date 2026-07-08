"""데스크톱 앱 런처.

로컬 FastAPI 서버를 백그라운드 스레드로 띄우고, pywebview 로 자체 창을 연다.
pywebview 가 없으면 기본 브라우저로 폴백한다. (옵션 ⓐ: 창을 닫으면 종료)
"""

from __future__ import annotations

import json
import logging
import os
import socket
import threading
import time
from pathlib import Path
from urllib.request import urlopen

log = logging.getLogger("worklog")


# --------------------------------------------------------------------------- #
# 단일 인스턴스 (중복 실행 방지 + 기존 창 앞으로)
# --------------------------------------------------------------------------- #

_MUTEX_NAME = "worklog_generator_singleton"


def _acquire_single_instance():
    """Windows 네임드 뮤텍스로 단일 인스턴스 확보. 반환 (is_first, handle).

    이미 실행 중이면 (False, None). 타 OS·실패 시 (True, None)(단일화 미적용).
    """
    if os.name != "nt":
        return True, None
    try:
        import ctypes
        handle = ctypes.windll.kernel32.CreateMutexW(None, False, _MUTEX_NAME)
        already = ctypes.windll.kernel32.GetLastError() == 183  # ERROR_ALREADY_EXISTS
        if already:
            if handle:
                ctypes.windll.kernel32.CloseHandle(handle)
            return False, None
        return True, handle
    except Exception:  # noqa: BLE001
        return True, None


def _release_single_instance(handle) -> None:
    if handle:
        try:
            import ctypes
            ctypes.windll.kernel32.CloseHandle(handle)
        except Exception:  # noqa: BLE001
            pass


def _instance_file() -> Path:
    return Path.home() / ".worklog" / "instance.json"


def _write_instance_file(port: int) -> None:
    try:
        p = _instance_file()
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps({"port": port}), encoding="utf-8")
    except OSError:
        pass


def _clear_instance_file() -> None:
    try:
        _instance_file().unlink()
    except OSError:
        pass


def _focus_existing() -> None:
    """기존 인스턴스의 /api/show 를 불러 창을 앞으로 띄운다."""
    try:
        info = json.loads(_instance_file().read_text(encoding="utf-8"))
        port = info.get("port")
    except (OSError, ValueError, AttributeError):
        return
    if not port:
        return
    try:
        urlopen(f"http://127.0.0.1:{port}/api/show", timeout=2)
    except Exception:  # noqa: BLE001
        pass


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_until_up(url: str, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urlopen(url, timeout=1)
            return True
        except Exception:
            time.sleep(0.15)
    return False


def run_app(config_path: str | None = None) -> int:
    try:
        import uvicorn
    except ImportError:
        print('앱 실행에는 추가 패키지가 필요합니다:  pip install "worklog-generator[app]"')
        return 1

    # 단일 인스턴스: 이미 실행 중이면 기존(트레이) 창을 앞으로 하고 이 인스턴스는 종료.
    is_first, mutex_handle = _acquire_single_instance()
    if not is_first:
        log.info("이미 실행 중입니다 → 기존 창을 앞으로 가져옵니다.")
        _focus_existing()
        return 0

    from .server import create_app, set_show_callback

    app = create_app(config_path)
    port = _free_port()
    url = f"http://127.0.0.1:{port}"

    config = uvicorn.Config(app, host="127.0.0.1", port=port, log_level="warning")
    server = uvicorn.Server(config)
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()

    if not _wait_until_up(url):
        print("로컬 서버 시작 실패")
        server.should_exit = True
        _release_single_instance(mutex_handle)
        return 1

    _write_instance_file(port)   # 두 번째 실행이 이 포트로 /api/show 호출
    log.info("업무일지 앱: %s", url)

    # 1순위: pywebview 자체 창. 미설치(ImportError)거나 GUI 초기화 실패면 브라우저로 폴백.
    used_gui = False
    window = None
    try:
        import webview

        # text_select=True: 본문(요약·표·타임라인)을 마우스로 선택·복사할 수 있게.
        # (pywebview 기본값 False 는 body 전체 선택을 막아 문서 뷰에 부적절)
        window = webview.create_window("업무일지", url, width=960, height=780,
                                       min_size=(780, 560), text_select=True)
        used_gui = True
    except ImportError:
        pass
    except Exception as e:  # noqa: BLE001
        log.warning("pywebview 창 생성 실패 → 브라우저로 폴백: %s", e)

    if used_gui:
        from .server import set_pick_path_callback

        set_show_callback(lambda: window.show())      # 두 번째 실행 → 이 창을 앞으로
        set_pick_path_callback(lambda mode, ft: _pick_path(webview, window, mode, ft))  # 경로 선택창
        tray = _setup_tray(webview, window, server)   # 닫으면 트레이로(가능하면)
        log.info("시스템 트레이: %s", "활성(닫으면 트레이로)" if tray else "미지원(닫으면 종료)")
        try:
            webview.start()  # 창을 닫을 때까지(트레이면 '종료' 누를 때까지) 블록
        except Exception as e:  # noqa: BLE001
            log.warning("pywebview 실행 실패 → 브라우저로 폴백: %s", e)
            used_gui = False

    if not used_gui:
        import webbrowser

        webbrowser.open(url)
        print(f"업무일지 앱이 브라우저에서 열렸습니다: {url}")
        print("(pywebview 가 창을 못 띄우는 환경이라 브라우저로 열었습니다. 종료: Ctrl+C)")
        try:
            while True:
                time.sleep(1)
        except KeyboardInterrupt:
            pass

    server.should_exit = True
    _clear_instance_file()
    _release_single_instance(mutex_handle)
    return 0


def _pick_path(webview, window, mode="folder", file_types=None) -> str | None:
    """네이티브 폴더/파일 선택창을 열어 고른 경로를 돌려준다. 취소면 None."""
    # pywebview 5.4+ 는 FileDialog.*, 이전은 *_DIALOG(deprecated) — 둘 다 대응.
    fd = getattr(webview, "FileDialog", None)

    def _open(ft):
        if mode == "file":
            dtype = fd.OPEN if fd else webview.OPEN_DIALOG
            kwargs = {"file_types": tuple(ft)} if ft else {}
            return window.create_file_dialog(dtype, **kwargs)
        dtype = fd.FOLDER if fd else webview.FOLDER_DIALOG
        return window.create_file_dialog(dtype)

    try:
        result = _open(file_types)
    except Exception as e:  # noqa: BLE001  파일 필터 형식 문제면 필터 없이라도 연다
        log.warning("경로 선택 다이얼로그(필터 %r) 실패 → 필터 없이 재시도: %s", file_types, e)
        try:
            result = _open(None)
        except Exception as e2:  # noqa: BLE001
            log.warning("경로 선택 다이얼로그 실패: %s", e2)
            return None
    if not result:
        return None
    return result[0] if isinstance(result, (list, tuple)) else str(result)


def _setup_tray(webview, window, server) -> bool:
    """닫기 시 창을 시스템 트레이로 최소화하고 트레이 메뉴(열기/생성/종료)를 단다.

    pystray/pillow 가 없으면 미설치 → 닫기 = 종료(기본). 트레이가 없으면 창을 숨기지 않아,
    창을 못 되찾는 상황을 만들지 않는다.
    """
    try:
        import pystray
        from PIL import Image, ImageDraw
    except ImportError:
        return False

    state = {"quitting": False}

    def on_closing():
        if state["quitting"]:
            return True          # '종료' 중 → 실제로 닫힘
        try:
            window.hide()        # 창만 숨겨 트레이로
        except Exception:        # noqa: BLE001
            return True          # 숨기기 실패하면 그냥 닫히게(갇힘 방지)
        return False             # 닫기 취소

    try:
        window.events.closing += on_closing
    except Exception as e:       # noqa: BLE001
        log.warning("트레이 닫기 훅 설치 실패 → 닫기=종료: %s", e)
        return False

    image = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle([10, 6, 54, 58], radius=8, fill=(91, 81, 192, 255))
    for y in (20, 30, 40):
        draw.line([20, y, 44, y], fill="white", width=4)

    def _open(icon, item):
        try:
            window.show()
        except Exception:        # noqa: BLE001
            pass

    def _generate(icon, item):
        try:
            window.show()
            window.evaluate_js("var b=document.getElementById('gen'); if(b){b.click();}")
        except Exception:        # noqa: BLE001
            pass

    def _quit(icon, item):
        state["quitting"] = True
        server.should_exit = True
        try:
            icon.stop()
        except Exception:        # noqa: BLE001
            pass
        try:
            window.destroy()
        except Exception:        # noqa: BLE001
            pass

    menu = pystray.Menu(
        pystray.MenuItem("열기", _open, default=True),
        pystray.MenuItem("오늘 업무일지 생성", _generate),
        pystray.Menu.SEPARATOR,
        pystray.MenuItem("종료", _quit),
    )
    icon = pystray.Icon("worklog", image, "업무일지", menu)
    threading.Thread(target=icon.run, daemon=True).start()
    return True


def main() -> int:
    import argparse

    from ..util import setup_logging

    parser = argparse.ArgumentParser(prog="worklog-app", description="업무일지 데스크톱 앱")
    parser.add_argument("--config", help="config.yaml 경로")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    setup_logging(args.verbose)
    return run_app(config_path=args.config)


if __name__ == "__main__":
    raise SystemExit(main())
