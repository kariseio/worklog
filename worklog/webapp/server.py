"""로컬 FastAPI 서버 — 앱 UI 에 데이터를 제공한다.

전부 127.0.0.1 로컬 전용. 인증 없음(로컬 단일 사용자). 외부로 나가는 것 없음.
"""

from __future__ import annotations

import logging
from datetime import date
from pathlib import Path

from .. import service
from ..collectors.naverworks import list_calendars as nw_list_calendars
from ..collectors.naverworks import test_connection as nw_test
from ..config import (
    Config,
    NaverWorksConfig,
    NotionOutputConfig,
    ObsidianOutputConfig,
    load_app_settings,
    load_config,
    save_app_settings,
)
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


def set_show_callback(fn) -> None:
    global _show_cb
    _show_cb = fn


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
            render_timeline_for_llm,
            render_work_signal,
        )

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

    # ---- 설정 (앱 내부에서 저장/불러오기, 비밀 값은 마스킹) ----

    @app.get("/api/settings")
    def get_settings():
        c = cfg()
        o, n, w = c.outputs.obsidian, c.outputs.notion, c.naverworks
        nw_store = (load_app_settings().get("sources") or {}).get("naverworks") or {}
        return {
            "timezone": c.timezone,
            "include_raw_data": c.include_raw_data,
            "summarizer": {"provider": c.summarizer.provider, "model": c.summarizer.model},
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
