"""오케스트레이션 서비스 — CLI 와 데스크톱 앱이 공유하는 핵심 로직.

수집기 실행 순서/결합, 요약, 문서 조합, 저장, 히스토리 조회를 한곳에 모아
cli.py 와 webapp/server.py 가 동일하게 호출한다.
"""

from __future__ import annotations

import glob
import hashlib
import logging
import os
import re
from dataclasses import dataclass, field
from datetime import date

from .collectors.base import CollectContext
from .collectors.claude_logs import ClaudeLogCollector
from .collectors.codex_logs import CodexCollector
from .collectors.git_repos import GitCollector
from .collectors.naverworks import NaverWorksCollector
from .config import Config
from .models import (
    CalendarData,
    ClaudeData,
    CodexData,
    DailyData,
    GitData,
    WorkLog,
)
from .outputs.base import SinkResult
from .outputs.markdown import MarkdownSink
from .outputs.notion import NotionSink
from .outputs.obsidian import ObsidianSink
from .analyze import analyze
from .render import (
    _is_meta_session,
    render_analysis,
    render_facts,
    render_session_blocks,
    render_session_section,
    render_timeline_for_llm,
    render_work_signal,
)
from .summarize import summarize_day
from .util import get_tz, resolve_day

log = logging.getLogger("worklog")

ALL_SOURCES = {"git", "claude", "codex", "naverworks"}


@dataclass
class SourceStatus:
    name: str
    state: str          # ok | skipped | error | disabled
    count: int = 0
    note: str | None = None


@dataclass
class GenerateResult:
    worklog: WorkLog
    statuses: list[SourceStatus] = field(default_factory=list)


# --------------------------------------------------------------------------- #
# 컨텍스트
# --------------------------------------------------------------------------- #


def make_context(cfg: Config, date_str: str | None) -> tuple[CollectContext, object, date]:
    tz = get_tz(cfg.timezone)
    target, start, end = resolve_day(date_str, tz)
    ctx = CollectContext(
        target_date=target, start=start, end=end, tz=tz, tz_name=cfg.timezone, logger=log
    )
    return ctx, tz, target


def enabled_sources(cfg: Config, sources_arg: str | list[str] | None) -> set[str]:
    result = {
        name for name, on in [
            ("git", cfg.git.enabled),
            ("claude", cfg.claude.enabled),
            ("codex", cfg.codex.enabled),
            ("naverworks", cfg.naverworks.enabled),
        ] if on
    }
    if sources_arg:
        if isinstance(sources_arg, str):
            requested = {s.strip() for s in sources_arg.split(",") if s.strip()}
        else:
            requested = {s.strip() for s in sources_arg if s and s.strip()}
        result = requested & ALL_SOURCES
    return result


# --------------------------------------------------------------------------- #
# 수집
# --------------------------------------------------------------------------- #


def collect(cfg: Config, ctx: CollectContext, sources: set[str]) -> tuple[DailyData, list[SourceStatus]]:
    data = DailyData(target_date=ctx.target_date, tz_name=cfg.timezone)
    statuses: dict[str, SourceStatus] = {
        n: SourceStatus(name=n, state="disabled") for n in ALL_SOURCES
    }
    counters = {
        # 렌더와 일치하도록 자동요약(meta) 세션은 세지 않는다(칩 수 = 실제 표시 세션 수).
        "claude": lambda d: sum(1 for s in d.sessions if not _is_meta_session(s)) if d else 0,
        "codex": lambda d: sum(1 for s in d.sessions if not _is_meta_session(s)) if d else 0,
        "git": lambda d: len(d.commits) if d else 0,
        "naverworks": lambda d: len(d.events) if d else 0,
    }

    def _assign(name: str, res) -> None:
        _apply_warnings(name, res, data)
        statuses[name] = _status(name, res, counters[name])

    # 1) Claude 로그 먼저 (git 자동탐색에 cwd 를 넘겨줘야 하므로 순서 고정)
    claude_cwds: list[str] = []
    if "claude" in sources:
        res = _safe_collect(ClaudeLogCollector(cfg.claude), ctx)
        _assign("claude", res)
        if isinstance(res.data, ClaudeData):
            data.claude = res.data
            claude_cwds = res.data.cwds

    # 2) git · codex · naverworks 는 서로 독립 → 병렬 실행
    #    (느린 git 이 네트워크 붙는 naverworks·파일 읽는 codex 와 시간상 겹쳐 돈다)
    parallel: list[tuple[str, object]] = []
    if "git" in sources:
        parallel.append(("git", GitCollector(cfg.git, extra_repos=claude_cwds)))
    if "codex" in sources:
        parallel.append(("codex", CodexCollector(cfg.codex)))
    if "naverworks" in sources:
        parallel.append(("naverworks", NaverWorksCollector(cfg.naverworks)))

    for name, res in _collect_parallel(parallel, ctx):
        _assign(name, res)
        if name == "git" and isinstance(res.data, GitData):
            data.git = res.data
        elif name == "codex" and isinstance(res.data, CodexData):
            data.codex = res.data
        elif name == "naverworks" and isinstance(res.data, CalendarData):
            data.calendar = res.data

    disambiguate_repo_names(data)

    order = ["git", "claude", "codex", "naverworks"]
    return data, [statuses[n] for n in order]


def _collect_parallel(items: list[tuple[str, object]], ctx: CollectContext):
    """(이름, 수집기) 들을 병렬 실행하고 (이름, 결과) 를 입력 순서대로 돌려준다."""
    if not items:
        return []
    if len(items) == 1:
        name, collector = items[0]
        return [(name, _safe_collect(collector, ctx))]
    from concurrent.futures import ThreadPoolExecutor

    with ThreadPoolExecutor(max_workers=len(items)) as ex:
        results = list(ex.map(lambda it: _safe_collect(it[1], ctx), items))
    return [(items[i][0], results[i]) for i in range(len(items))]


def disambiguate_repo_names(data: DailyData) -> None:
    """동명(basename 동일)이지만 물리적으로 다른 저장소를 상위 폴더로 구분한다.

    예: D:\\kms\\private\\kms_frontend 와 D:\\works\\kms_frontend 는 둘 다 'kms_frontend'
    → 'private/kms_frontend', 'works/kms_frontend' 로 구분. git 커밋과 Claude 세션이
    동일한 키(git-common-dir)를 공유하므로 서로 어긋나지 않는다.
    """
    from collections import defaultdict

    from .util import git_common_dir, repo_root_of

    key_base: dict[str, str] = {}          # 저장소키 → basename
    if data.git:
        for c in data.git.commits:
            if c.repo_path:
                key_base[c.repo_path] = c.repo
    sess_key: dict[int, str | None] = {}   # 세션 → 저장소키
    all_sessions = data.all_sessions       # Claude + Codex
    for s in all_sessions:
        k = git_common_dir(s.cwd) if s.cwd else None
        sess_key[id(s)] = k
        if k:
            key_base.setdefault(k, s.project or "?")

    keys_by_base: dict[str, set] = defaultdict(set)
    for key, base in key_base.items():
        keys_by_base[base].add(key)

    newname: dict[str, str] = {}
    for base, keys in keys_by_base.items():
        if len(keys) <= 1:
            for k in keys:
                newname[k] = base
            continue
        # 충돌 → 상위 폴더로 구분
        cand = {}
        for k in keys:
            parent = os.path.basename(os.path.dirname(repo_root_of(k)))
            cand[k] = f"{parent}/{base}" if parent else base
        # 상위 폴더까지 같아 여전히 겹치면 짧은 해시로 유일화
        dup: dict[str, list] = defaultdict(list)
        for k, nm in cand.items():
            dup[nm].append(k)
        for nm, ks in dup.items():
            if len(ks) == 1:
                newname[ks[0]] = nm
            else:
                for k in ks:
                    h = hashlib.sha1(k.encode("utf-8")).hexdigest()[:6]
                    newname[k] = f"{nm}#{h}"

    if data.git:
        for c in data.git.commits:
            if c.repo_path in newname:
                c.repo = newname[c.repo_path]
    for s in all_sessions:
        k = sess_key.get(id(s))
        if k in newname:
            s.project = newname[k]


def _safe_collect(collector, ctx: CollectContext):
    """수집기를 실행하되 예외를 삼켜 항상 CollectorResult 를 돌려준다(스레드 안전, data 미접근)."""
    from .collectors.base import CollectorResult

    try:
        return collector.collect(ctx)
    except Exception as e:  # noqa: BLE001
        log.warning("[%s] 예외: %s", collector.name, e)
        return CollectorResult.fail(collector.name, f"예외: {e}")


def _apply_warnings(name: str, result, data: DailyData) -> None:
    """수집 결과의 경고/건너뜀을 data 에 반영·로깅한다(메인 스레드에서 호출)."""
    for w in result.warnings:
        log.warning("[%s] %s", name, w)
        data.warnings.append(f"[{name}] {w}")
    if result.skipped:
        log.info("[%s] 건너뜀: %s", name, result.skip_reason)
        data.warnings.append(f"[{name}] 건너뜀: {result.skip_reason}")


def _status(name: str, result, counter) -> SourceStatus:
    if result.skipped:
        return SourceStatus(name=name, state="skipped", count=0, note=result.skip_reason)
    if not result.ok:
        return SourceStatus(name=name, state="error", count=0,
                            note=(result.warnings[0] if result.warnings else "실패"))
    return SourceStatus(name=name, state="ok", count=counter(result.data),
                        note=(result.warnings[0] if result.warnings else None))


# --------------------------------------------------------------------------- #
# 문서 조합 / 요약 / 생성
# --------------------------------------------------------------------------- #


def availability_line(cfg: Config, statuses: list[SourceStatus]) -> str:
    """수집 소스 + 저장 대상의 유무를 한눈에 보여주는 라벨. (생성 결과가 유무에 따라 달라짐을 명시)"""
    icon = {"ok": "✅", "skipped": "❌", "error": "⚠️", "disabled": "❌"}
    src_label = {
        "git": "Git", "claude": "Claude", "codex": "Codex",
        "naverworks": "캘린더(NaverWorks)",
    }
    order = {"git": 0, "claude": 1, "codex": 2, "naverworks": 3}
    src = []
    for s in sorted(statuses, key=lambda x: order.get(x.name, 9)):
        cnt = f"({s.count})" if s.state == "ok" and s.count else ""
        src.append(f"{src_label.get(s.name, s.name)} {icon.get(s.state, '?')}{cnt}")

    # 실제 저장은 .enabled 로 결정되므로(save() 와 일치), '켜짐 그리고 설정됨'을 기준으로 표기.
    ob = cfg.outputs.obsidian
    no = cfg.outputs.notion
    out = [
        f"로컬 md {'✅' if cfg.outputs.markdown.enabled else '❌'}",
        f"Obsidian {'✅' if (ob.enabled and ob.vault_dir) else '❌'}",
        f"Notion {'✅' if (no.enabled and no.token and no.parent_id) else '❌'}",
    ]
    return "수집 소스: " + " · ".join(src) + "\n저장 대상: " + " · ".join(out)


def compose_full(target: date, summary: str | None, facts: str,
                 availability: str = "", analysis_md: str = "",
                 include_raw: bool = False) -> str:
    lines = [f"# 📝 업무일지 {target.isoformat()}", ""]
    # (수집 소스·저장 대상 표기는 문서에 넣지 않음 — 앱 화면 칩으로만 표시)
    if summary:
        lines.append(summary.strip())
    else:
        lines.append("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요.")
    if analysis_md and analysis_md.strip():
        lines += ["", analysis_md.strip()]
    if include_raw:
        lines += [
            "", "---", "",
            "<details>", "<summary>📊 수집 데이터 원본</summary>", "",
            facts.strip(), "", "</details>", "",
        ]
    else:
        lines.append("")
    return "\n".join(lines)


def summarize_signal(cfg: Config, signal: str, target: date, availability: str = "") -> str | None:
    # 신호에 세션 질답 섹션이 있으면 summarize_day 가 필요시 map-reduce 로 안전 처리.
    return summarize_day(signal, target.isoformat(), cfg.summarizer, availability)


def generate(cfg: Config, date_str: str | None = None,
             no_llm: bool = False, sources: str | list[str] | None = None) -> GenerateResult:
    """수집 → (정제 신호) 요약 → 조합까지 한 번에. (저장은 별도)"""
    ctx, tz, target = make_context(cfg, date_str)
    data, statuses = collect(cfg, ctx, enabled_sources(cfg, sources))
    facts = render_facts(data, tz)
    avail = availability_line(cfg, statuses)
    an = analyze(data, tz)
    analysis_md = render_analysis(an)
    signal = render_work_signal(data, tz, header="가용 데이터 — " + avail.replace("\n", " / "))
    timeline_text = render_timeline_for_llm(an)
    if timeline_text:
        signal = signal + "\n" + timeline_text
    session_section = render_session_section(render_session_blocks(data, tz))
    if session_section:
        signal = signal.rstrip() + "\n\n" + session_section

    summary = None
    if not no_llm and not data.is_empty():
        summary = summarize_signal(cfg, signal, target, avail)

    full = compose_full(target, summary, facts, avail, analysis_md, cfg.include_raw_data)
    worklog = WorkLog(target_date=target, facts_markdown=facts, full_markdown=full,
                      data=data, summary_markdown=summary)
    return GenerateResult(worklog=worklog, statuses=statuses)


# --------------------------------------------------------------------------- #
# 저장
# --------------------------------------------------------------------------- #


def save(cfg: Config, worklog: WorkLog, targets: list[str] | None = None) -> list[SinkResult]:
    """targets 로 지정된 곳(또는 config 에서 enabled 된 곳)에 저장."""
    sinks = []
    want = set(targets) if targets else None

    def enabled(name: str, cfg_enabled: bool) -> bool:
        return (name in want) if want is not None else cfg_enabled

    if enabled("markdown", cfg.outputs.markdown.enabled):
        sinks.append(MarkdownSink(cfg.outputs.markdown))
    if enabled("obsidian", cfg.outputs.obsidian.enabled):
        sinks.append(ObsidianSink(cfg.outputs.obsidian))
    if enabled("notion", cfg.outputs.notion.enabled):
        sinks.append(NotionSink(cfg.outputs.notion))

    return [sink.write(worklog) for sink in sinks]


# --------------------------------------------------------------------------- #
# 히스토리 (저장된 markdown 파일 기준)
# --------------------------------------------------------------------------- #

_DATE_RE = re.compile(r"^(\d{4}-\d{2}-\d{2})\.md$")


def list_history(cfg: Config) -> list[str]:
    out_dir = os.path.expanduser(cfg.outputs.markdown.dir)
    dates: list[str] = []
    for path in glob.glob(os.path.join(out_dir, "*.md")):
        m = _DATE_RE.match(os.path.basename(path))
        if m:
            dates.append(m.group(1))
    return sorted(set(dates), reverse=True)


def read_saved(cfg: Config, date_str: str) -> str | None:
    # 경로 traversal 방지: YYYY-MM-DD 형식만 허용
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", date_str or ""):
        return None
    out_dir = os.path.expanduser(cfg.outputs.markdown.dir)
    path = os.path.join(out_dir, f"{date_str}.md")
    if not os.path.isfile(path):
        return None
    try:
        with open(path, encoding="utf-8") as f:
            return f.read()
    except OSError:
        return None


# --------------------------------------------------------------------------- #
# 앱 UI 용 근거(evidence) 추출
# --------------------------------------------------------------------------- #


def to_evidence(data: DailyData, tz) -> dict:
    from .util import fmt_time, parse_iso

    ev: dict = {"git": [], "claude": [], "codex": [], "calendar": []}

    def _session_ev(s) -> dict:
        return {
            "project": s.project, "branch": s.git_branch,
            "title": s.title or s.intent, "intent": s.intent,
            "files": len(s.files_edited), "tokens": s.output_tokens,
            "tools": s.tool_counts,
        }

    if data.git:
        for c in data.git.commits:
            ev["git"].append({
                "repo": c.repo, "hash": c.short_hash, "subject": c.subject,
                "insertions": c.insertions, "deletions": c.deletions,
                "files": c.files_changed,
            })
    # 칩 수·지표·본문과 일치하도록 자동요약(meta) 세션은 근거에서도 제외한다.
    if data.claude:
        for s in data.claude.sessions:
            if not _is_meta_session(s):
                ev["claude"].append(_session_ev(s))
    if data.codex:
        for s in data.codex.sessions:
            if not _is_meta_session(s):
                ev["codex"].append(_session_ev(s))
    if data.calendar:
        for e in data.calendar.events:
            when = "종일" if e.all_day else f"{fmt_time(parse_iso(e.start), tz)}–{fmt_time(parse_iso(e.end), tz)}"
            ev["calendar"].append({
                "title": e.title, "when": when, "location": e.location,
                "attendees": len(e.attendees),
            })
    return ev
