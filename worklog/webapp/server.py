"""로컬 FastAPI 서버 — 앱 UI 에 데이터를 제공한다.

전부 127.0.0.1 로컬 전용. 인증 없음(로컬 단일 사용자). 외부로 나가는 것 없음.
"""

from __future__ import annotations

import logging
import os
import subprocess
import sys
from datetime import date
from pathlib import Path

from .. import __version__, service
from ..collectors.naverworks import list_calendars as nw_list_calendars
from ..collectors.naverworks import test_connection as nw_test
from ..config import (
    Config,
    NaverWorksConfig,
    NotionOutputConfig,
    ObsidianOutputConfig,
    app_settings_path,
    load_app_settings,
    load_config,
    save_app_settings,
)
from ..util import drives_info
from ..models import DailyData, WorkLog
from ..outputs.notion import test_connection as notion_test
from ..outputs.obsidian import test_connection as obsidian_test

log = logging.getLogger("worklog")

try:
    from fastapi import Body, FastAPI, Query
    from fastapi.responses import FileResponse, JSONResponse
    from pydantic import BaseModel
except ImportError as e:  # pragma: no cover
    raise RuntimeError(
        '앱 실행에는 추가 패키지가 필요합니다: pip install "worklog-generator[app]"'
    ) from e

STATIC = Path(__file__).resolve().parent / "static"

# 단일 인스턴스: 두 번째 실행이 /api/show 를 부르면 기존 창을 앞으로 띄운다.
_show_cb = None
# 데스크톱(pywebview) 네이티브 경로 선택 다이얼로그. 브라우저 폴백에선 None.
_pick_cb = None


def set_show_callback(fn) -> None:
    global _show_cb
    _show_cb = fn


def set_pick_path_callback(fn) -> None:
    """fn(mode, file_types) -> 선택 경로(str) | None. mode: 'folder' | 'file'."""
    global _pick_cb
    _pick_cb = fn


def _root_definitely_gone(root: str) -> bool:
    """루트가 '확실히 삭제됨'인지. 드라이브/공유는 살아있는데 폴더만 없을 때만 True.

    드라이브 자체가 미마운트(외장 SSD·USB·네트워크 일시 분리)면 False → 보존.
    """
    p = os.path.expanduser(str(root))
    if os.path.isdir(p):
        return False                       # 존재 → 삭제 아님
    anchor = os.path.splitdrive(p)[0]      # 'E:' 또는 '\\\\server\\share'
    if not anchor:
        return True                        # 앵커 없는 경로 → 판단 불가, 없으면 삭제 취급
    if not os.path.exists(anchor + os.sep):
        return False                       # 드라이브/공유 미마운트 → 일시 부재로 보존
    return True                            # 드라이브는 있는데 폴더만 없음 → 진짜 삭제


def _prune_missing_scan_roots() -> list[str]:
    """캐시(settings.json)의 git 스캔 루트 중 실제로 없어진 폴더를 제거한다.

    제거된 경로 목록을 돌려준다(없으면 빈 리스트). config.yaml 쪽은 건드리지 않는다.
    """
    store = load_app_settings()
    sources = store.get("sources")
    if not (isinstance(sources, dict) and isinstance(sources.get("git"), dict)):
        return []
    roots = sources["git"].get("scan_roots")
    if not isinstance(roots, list) or not roots:
        return []
    kept, removed = [], []
    for r in roots:
        (removed if (not r or _root_definitely_gone(r)) else kept).append(r)
    if removed:
        sources["git"]["scan_roots"] = kept
        try:
            save_app_settings(store)
        except OSError:   # 저장 실패해도 생성은 계속(캐시 정리는 다음 기회에)
            return []
    return [r for r in removed if r]


class SummarizeReq(BaseModel):
    date: str
    signal_markdown: str = ""
    availability: str = ""


class SaveReq(BaseModel):
    date: str
    summary_markdown: str | None = None
    facts_markdown: str = ""
    availability: str = ""
    analysis_md: str = ""
    targets: list[str] = []


def create_app(config_path: str | None = None) -> "FastAPI":
    app = FastAPI(title="업무일지", docs_url=None, redoc_url=None)

    def cfg() -> Config:
        # 매 요청마다 로드해 config.yaml/.env 변경을 즉시 반영.
        return load_config(config_path)

    @app.get("/")
    def index():
        return FileResponse(STATIC / "index.html")

    @app.get("/api/show")
    def show():
        # 두 번째 인스턴스가 호출 → 기존(트레이) 창을 앞으로.
        if _show_cb:
            try:
                _show_cb()
            except Exception:  # noqa: BLE001
                pass
        return {"ok": True}

    @app.get("/api/status")
    def status():
        c = cfg()
        return {
            "version": __version__,
            "timezone": c.timezone,
            "enabled_sources": sorted(service.enabled_sources(c, None)),
            "summarizer": {"provider": c.summarizer.provider, "model": c.summarizer.model},
            "outputs": {
                "markdown": c.outputs.markdown.enabled,
                "obsidian": bool(c.outputs.obsidian.vault_dir),
                "notion": bool(c.outputs.notion.token and c.outputs.notion.parent_id),
            },
        }

    @app.get("/api/collect")
    def collect(date_str: str | None = Query(None, alias="date"),
                sources: str | None = Query(None)):
        from ..analyze import analyze
        from ..render import (
            render_analysis,
            render_facts,
            render_session_blocks,
            render_session_section,
            render_timeline_for_llm,
            render_work_signal,
        )

        _prune_missing_scan_roots()   # 생성 시점에 사라진 스캔 루트는 캐시에서 제거
        c = cfg()
        try:
            ctx, tz, target = service.make_context(c, date_str)
        except ValueError:
            ctx, tz, target = service.make_context(c, None)   # 잘못된 date → 오늘
        data, statuses = service.collect(c, ctx, service.enabled_sources(c, sources))
        facts = render_facts(data, tz)
        avail = service.availability_line(c, statuses)
        an = analyze(data, tz)
        signal = render_work_signal(data, tz, header="가용 데이터 — " + avail.replace("\n", " / "))
        tl = render_timeline_for_llm(an)
        if tl:
            signal = signal + "\n" + tl
        session_section = render_session_section(render_session_blocks(data, tz))
        if session_section:
            signal = signal.rstrip() + "\n\n" + session_section
        return {
            "date": target.isoformat(),
            "facts_markdown": facts,
            "signal_markdown": signal,
            "availability": avail,
            "analysis": an.to_dict(),
            "analysis_md": render_analysis(an),
            "evidence": service.to_evidence(data, tz),
            "statuses": [s.__dict__ for s in statuses],
            "empty": data.is_empty(),
        }

    @app.post("/api/summarize")
    def summarize(req: SummarizeReq):
        c = cfg()
        try:
            target = date.fromisoformat(req.date)
        except ValueError:
            target = service.make_context(c, None)[2]
        summary = service.summarize_signal(c, req.signal_markdown, target, req.availability)
        return {"summary_markdown": summary, "provider": c.summarizer.provider}

    @app.post("/api/save")
    def save(req: SaveReq):
        c = cfg()
        try:
            target = date.fromisoformat(req.date)
        except ValueError:
            target = service.make_context(c, None)[2]
        full = service.compose_full(target, req.summary_markdown, req.facts_markdown,
                                    req.availability, req.analysis_md, c.include_raw_data)
        worklog = WorkLog(
            target_date=target, facts_markdown=req.facts_markdown, full_markdown=full,
            data=DailyData(target_date=target, tz_name=c.timezone),
            summary_markdown=req.summary_markdown,
        )
        results = service.save(c, worklog, targets=req.targets or None)
        return {"results": [r.__dict__ for r in results]}

    @app.get("/api/history")
    def history():
        return {"dates": service.list_history(cfg())}

    @app.get("/api/worklog")
    def worklog(date_str: str = Query(..., alias="date")):
        md = service.read_saved(cfg(), date_str)
        if md is None:
            return JSONResponse({"markdown": None}, status_code=404)
        return {"markdown": md}

    @app.post("/api/open-folder")
    def open_folder(body: dict = Body(default={})):
        """저장 폴더를 OS 파일 탐색기로 연다. 로컬 데스크톱 앱 전용."""
        raw = (body or {}).get("path") or cfg().outputs.markdown.dir
        target = Path(os.path.expanduser(raw))
        try:
            target.mkdir(parents=True, exist_ok=True)
        except OSError as e:
            return JSONResponse({"ok": False, "message": f"폴더 생성 실패: {e}"}, status_code=400)
        try:
            if os.name == "nt":
                os.startfile(str(target))  # noqa: S606
            elif sys.platform == "darwin":
                subprocess.run(["open", str(target)], check=False)
            else:
                subprocess.run(["xdg-open", str(target)], check=False)
        except Exception as e:  # noqa: BLE001
            return JSONResponse({"ok": False, "message": str(e)}, status_code=400)
        return {"ok": True, "path": str(target)}

    @app.get("/api/drives")
    def drives():
        """스캔 루트로 고를 수 있는 물리 볼륨 목록 [{path, label}]."""
        return {"drives": drives_info()}

    @app.post("/api/pick-path")
    def pick_path(body: dict = Body(default={})):
        """네이티브 폴더/파일 선택 다이얼로그. 데스크톱 앱에서만 지원."""
        if not _pick_cb:
            return JSONResponse(
                {"ok": False, "message": "경로 선택창은 데스크톱 앱에서만 지원됩니다. 직접 입력하세요."},
                status_code=400)
        mode = (body or {}).get("mode") or "folder"
        file_types = (body or {}).get("file_types") or []
        try:
            path = _pick_cb(mode, file_types)
        except Exception as e:  # noqa: BLE001
            return JSONResponse({"ok": False, "message": str(e)}, status_code=400)
        if not path:
            return {"ok": False, "cancelled": True}
        return {"ok": True, "path": str(path)}

    @app.get("/api/update/check")
    def update_check():
        """GitHub Releases 에서 최신 버전 확인."""
        from .. import update
        return update.check()

    _upd = {"staged": None}   # 다운로드해 둔 <exe>.new 경로(finalize 에서 교체에 사용)

    @app.post("/api/update/apply")
    def update_apply(body: dict = Body(default={})):
        """새 버전 exe 를 내려받아 교체 준비(staging)만 한다. 실제 닫기/교체는 finalize 에서.

        앱을 여기서 닫지 않는 이유: UI 가 '확인하면 닫고 적용' 알림을 띄운 뒤, 사용자가
        확인했을 때 finalize 로 닫도록 해서 갑작스런 종료를 막는다.
        """
        from .. import update
        if not update.is_frozen():
            return JSONResponse(
                {"ok": False, "message": "설치본(exe)에서만 업데이트할 수 있습니다."}, status_code=400)
        info = update.check()
        if not info.get("update_available") or not info.get("download_url"):
            return JSONResponse(
                {"ok": False, "message": "적용할 새 버전이 없습니다."}, status_code=400)
        try:
            _upd["staged"] = update.download_and_stage(info["download_url"])
        except Exception as e:  # noqa: BLE001
            return JSONResponse({"ok": False, "message": f"다운로드 실패: {e}"}, status_code=400)
        return {"ok": True, "staged": True, "version": info.get("latest")}

    @app.post("/api/update/finalize")
    def update_finalize(body: dict = Body(default={})):
        """staging 된 새 exe 로 교체 배치를 띄우고 앱을 닫는다(재실행 없음 → 사용자가 다시 실행)."""
        from .. import update
        new = _upd.get("staged")
        if not new or not os.path.exists(new):
            return JSONResponse(
                {"ok": False, "message": "적용할 다운로드가 없습니다. 다시 시도해 주세요."}, status_code=400)
        try:
            update.schedule_apply_and_restart(new)   # 교체 배치 + 잠시 뒤 앱 종료(재실행 안 함)
        except Exception as e:  # noqa: BLE001
            return JSONResponse({"ok": False, "message": f"적용 실패: {e}"}, status_code=400)
        return {"ok": True, "closing": True}

    # ---- 설정 (앱 내부에서 저장/불러오기, 비밀 값은 마스킹) ----

    @app.get("/api/settings")
    def get_settings():
        c = cfg()
        o, n, w = c.outputs.obsidian, c.outputs.notion, c.naverworks
        _srcs = load_app_settings().get("sources") or {}
        nw_store = _srcs.get("naverworks") or {}
        git_store = _srcs.get("git") or {}
        md_dir = os.path.expanduser(c.outputs.markdown.dir)
        return {
            "version": __version__,
            "timezone": c.timezone,
            "include_raw_data": c.include_raw_data,
            "summarizer": {"provider": c.summarizer.provider, "model": c.summarizer.model},
            "git": {
                "enabled": c.git.enabled,
                "scan_all_drives": bool(git_store.get("scan_all_drives", True)),   # 앱 기본: 모든 하드디스크
                "scan_roots": c.git.scan_roots,
                "scan_depth": c.git.scan_depth,
            },
            "activitywatch": {"enabled": c.activitywatch.enabled, "base_url": c.activitywatch.base_url},
            "claude": {"enabled": c.claude.enabled},
            "markdown": {"enabled": c.outputs.markdown.enabled, "dir": md_dir},
            "paths": {"settings_file": str(app_settings_path()), "save_dir": md_dir},
            "obsidian": {"enabled": o.enabled, "vault_dir": o.vault_dir, "subdir": o.subdir},
            "notion": {
                "enabled": n.enabled, "parent_type": n.parent_type, "parent_id": n.parent_id,
                "title_prop": n.title_prop, "version": n.version, "token_set": bool(n.token),
            },
            "naverworks": {
                "enabled": w.enabled, "user_id": w.user_id,
                "calendar_ids": w.calendar_ids,
                "calendars": nw_store.get("calendars") or [],   # 표시용 [{calendar_id,name}]
                "scope": w.scope, "client_id": w.client_id, "service_account": w.service_account,
                "private_key_path": w.private_key_path,
                "client_secret_set": bool(w.client_secret), "private_key_set": bool(w.private_key),
            },
        }

    @app.post("/api/settings")
    def set_settings(body: dict = Body(...)):
        store = load_app_settings()

        if body.get("timezone"):
            store["timezone"] = body["timezone"]
        if "include_raw_data" in body:
            store["include_raw_data"] = bool(body["include_raw_data"])
        if isinstance(body.get("summarizer"), dict):
            store["summarizer"] = {
                "provider": body["summarizer"].get("provider", "auto"),
                "model": body["summarizer"].get("model", "claude-opus-4-8"),
            }

        outs = store.setdefault("outputs", {})
        md = body.get("markdown")
        if isinstance(md, dict):
            mstore = outs.setdefault("markdown", {})
            mstore["enabled"] = bool(md.get("enabled", True))
            if md.get("dir"):          # 빈 값이면 기본(문서 폴더) 경로 유지
                mstore["dir"] = md["dir"]
        ob = body.get("obsidian")
        if isinstance(ob, dict):
            outs.setdefault("obsidian", {}).update({
                "enabled": bool(ob.get("enabled")),
                "vault_dir": ob.get("vault_dir", ""),
                "subdir": ob.get("subdir", "업무일지"),
            })
        no = body.get("notion")
        if isinstance(no, dict):
            target = outs.setdefault("notion", {})
            target.update({
                "enabled": bool(no.get("enabled")),
                "parent_type": no.get("parent_type", "page"),
                "parent_id": no.get("parent_id", ""),
                "title_prop": no.get("title_prop", "Name"),
            })
            if no.get("version"):    # UI 가 안 보내면 기존 값 유지
                target["version"] = no["version"]
            if no.get("token"):      # 빈 값이면 기존 토큰 유지
                target["token"] = no["token"]

        srcs = store.setdefault("sources", {})
        gt = body.get("git")
        if isinstance(gt, dict):
            try:
                depth = int(gt.get("scan_depth", 5))
            except (TypeError, ValueError):
                depth = 5
            depth = max(1, min(depth, 12))   # 폭주 방지(1~12단계)
            g = srcs.setdefault("git", {})
            g["enabled"] = bool(gt.get("enabled", True))
            g["scan_all_drives"] = bool(gt.get("scan_all_drives", True))
            g["scan_roots"] = gt.get("scan_roots") or []
            g["scan_depth"] = depth
            g["author"] = ""                # 앱: 작성자 필터 없음(내 커밋으로 간주)
            g["include_claude_cwds"] = True  # 앱: Claude 작업 폴더 항상 자동 포함
            # repos 는 앱에서 관리하지 않음 → 기존 값(config/CLI) 보존
        aw = body.get("activitywatch")
        if isinstance(aw, dict):
            srcs.setdefault("activitywatch", {}).update({
                "enabled": bool(aw.get("enabled", True)),
                "base_url": aw.get("base_url", "http://localhost:5600"),
            })
        cl = body.get("claude")
        if isinstance(cl, dict):
            srcs.setdefault("claude", {})["enabled"] = bool(cl.get("enabled", True))
        nw = body.get("naverworks")
        if isinstance(nw, dict):
            target = srcs.setdefault("naverworks", {})
            target.update({
                "enabled": bool(nw.get("enabled")),
                "user_id": nw.get("user_id", ""),
                "calendar_ids": nw.get("calendar_ids") or [],
                "client_id": nw.get("client_id", ""),
                "service_account": nw.get("service_account", ""),
                "private_key_path": nw.get("private_key_path", ""),
            })
            if "calendars" in nw:      # 표시용 이름 목록
                target["calendars"] = nw.get("calendars") or []
            if nw.get("scope"):        # UI 가 안 보내면 기존 값 유지
                target["scope"] = nw["scope"]
            if nw.get("client_secret"):
                target["client_secret"] = nw["client_secret"]
            if nw.get("private_key"):
                target["private_key"] = nw["private_key"]

        path = save_app_settings(store)
        return {"ok": True, "saved_to": str(path)}

    # ---- 연결 테스트 (실제 API/파일시스템으로 확인) ----

    @app.post("/api/test/obsidian")
    def test_obsidian(body: dict = Body(...)):
        ocfg = ObsidianOutputConfig(
            enabled=True, vault_dir=body.get("vault_dir", ""),
            subdir=body.get("subdir", "업무일지"),
        )
        ok, msg = obsidian_test(ocfg)
        return {"ok": ok, "message": msg}

    @app.post("/api/test/notion")
    def test_notion(body: dict = Body(...)):
        c = cfg()
        ncfg = NotionOutputConfig(
            enabled=True,
            token=body.get("token") or c.outputs.notion.token,
            parent_type=body.get("parent_type", "page"),
            parent_id=body.get("parent_id", ""),
            title_prop=body.get("title_prop", "Name"),
            version=body.get("version") or c.outputs.notion.version,
        )
        ok, msg = notion_test(ncfg)
        return {"ok": ok, "message": msg}

    @app.post("/api/test/naverworks")
    def test_naverworks(body: dict = Body(...)):
        c = cfg()
        wc = c.naverworks
        wcfg = NaverWorksConfig(
            enabled=True,
            user_id=body.get("user_id", ""),
            calendar_id=body.get("calendar_id", ""),
            scope=body.get("scope") or "calendar.read",
            client_id=body.get("client_id") or wc.client_id,
            service_account=body.get("service_account") or wc.service_account,
            client_secret=body.get("client_secret") or wc.client_secret,
            private_key=body.get("private_key") or wc.private_key,
            private_key_path=body.get("private_key_path") or wc.private_key_path,
        )
        ok, msg = nw_test(wcfg)
        return {"ok": ok, "message": msg}

    @app.post("/api/naverworks/calendars")
    def naverworks_calendars(body: dict = Body(...)):
        c = cfg()
        wc = c.naverworks
        wcfg = NaverWorksConfig(
            enabled=True,
            user_id=body.get("user_id") or wc.user_id,
            scope=body.get("scope") or wc.scope or "calendar.read",
            client_id=body.get("client_id") or wc.client_id,
            service_account=body.get("service_account") or wc.service_account,
            client_secret=body.get("client_secret") or wc.client_secret,
            private_key=body.get("private_key") or wc.private_key,
            private_key_path=body.get("private_key_path") or wc.private_key_path,
        )
        ok, result = nw_list_calendars(wcfg)
        if ok:
            return {"ok": True, "calendars": result}
        return {"ok": False, "message": result}

    return app
