"""LLM 종합(요약)기.

수집한 '사실 Markdown' 을 Claude 에게 주고 자연어 업무일지로 다듬는다.
provider:
  auto         → claude CLI 있으면 사용, 없으면 Anthropic API, 둘 다 없으면 None
  claude_cli   → 설치된 `claude` CLI (별도 API 키 불필요)
  anthropic_api→ Anthropic SDK (ANTHROPIC_API_KEY 또는 ant 프로필)
  none         → 요약 생략
"""

from __future__ import annotations

import functools
import logging
import shutil
import subprocess

from .config import SummarizerConfig
from .util import no_window_kwargs

log = logging.getLogger("worklog")


SYSTEM_KO = (
    "너는 하루치 개발 활동 로그(Claude Code 세션 질답 + git 커밋 + 일정 등)를 '업무일지'로 "
    "문서화하는 도구다. 목표 — '언제, 어떤 세션에서, 무엇을 다뤘고 무엇을 완료했는가'를 "
    "시간·세션 흐름대로 담는 것.\n\n"
    "규칙:\n"
    "1. 세션의 '질답 흐름'을 근거로 그날 다룬 주제·요청·결정·완료를 문서화한다. "
    "한 세션에서 여러 주제를 다뤘으면 그 주제들을 **모두** 반영한다(첫 주제만 쓰지 말 것).\n"
    "2. 각 항목은 한 줄, 개조식(명사구·완료형). 장황체·미사여구·불필요한 이모지 금지.\n"
    "3. 완료·결정된 것을 앞세우되, 중요한 '방향 전환·결정'도 한 줄로 남긴다. "
    "단 '~하려고 했다' 식 공허한 과정 나열은 피하고 결과·결정 중심으로.\n"
    "4. git 커밋을 완료 결과의 1차 근거로 삼는다. 원본 프롬프트·명령어·파일 목록을 그대로 나열하지 마라.\n"
    "5. 데이터에 없는 것은 지어내지 마라. 제공되지 않은 소스의 섹션은 만들지 마라.\n\n"
    "출력 구조:\n"
    "## 한 줄 요약 — 오늘을 한 문장으로.\n"
    "## 🕘 시간대별 흐름 — 시간순으로 자연스러운 블록(오전/점심/오후·저녁 또는 1~3시간)으로 묶어 "
    "**굵은 시간대**(예: **09–12시**) 아래 '몇 시경 무엇을 다뤘고 무엇을 했다'를 개조식으로. "
    "**회의(📅)는 반드시 해당 시각에 명시**한다.\n"
    "## 📁 프로젝트별 — 프로젝트로 묶어, 그 프로젝트에서 (여러 세션에 걸쳐) 다룬 주요 주제·진행·"
    "완료를 개조식으로 정리한다. **굵은 프로젝트명** 아래. 한 프로젝트를 여러 세션에서 다뤘으면 "
    "세션을 나누지 말고 합쳐서 정리하되, 필요하면 시간대를 괄호로 덧붙인다(예: (오전 노드제한, 오후 SSO)).\n\n"
    "전체가 한눈에 들어오게. 문단 쓰지 말고 불릿만 써라."
)

# 무거운 날: 세션마다 먼저 이걸로 개별 압축(map) 후, 압축본을 모아 SYSTEM_KO 로 종합(reduce).
CONDENSE_SYSTEM_KO = (
    "너는 Claude Code 한 세션의 '질답 흐름'을 요약하는 도구다. "
    "이 세션에서 사용자가 무엇을 요청·논의했고 무엇이 결정·완료됐는지를 시간 흐름을 살려 "
    "3~8줄 개조식으로 압축하라. 여러 주제를 다뤘으면 주제별로 한 줄씩. "
    "장황체·미사여구 금지, 결과·결정 중심. 완료된 변경은 파일/커밋 근거로 명확히. "
    "문단 쓰지 말고 불릿만."
)

USER_TEMPLATE_KO = (
    "{availability}\n\n"
    "아래는 {date} 활동 데이터(정제 신호 + 시간순 이벤트 + 세션 질답 흐름)다. 위 규칙대로 "
    "'시간대별 흐름'과 '프로젝트별 정리'를 담은 업무일지 본문만 출력하라. 원문을 그대로 나열하지 마라.\n\n"
    "---\n{signal}\n---\n"
)


@functools.lru_cache(maxsize=1)
def _claude_exe() -> str | None:
    """claude CLI 경로를 한 번만 조회(map-reduce 로 _call 이 수십 번 불려도 PATH 스캔 1회)."""
    return shutil.which("claude")


def _call(system: str, user: str, cfg: SummarizerConfig) -> str | None:
    """해석된 provider 로 (system, user) 한 번 호출. none/미지원이면 None."""
    provider = _resolve_provider(cfg.provider)
    if provider == "claude_cli":
        return _summarize_cli(system, user, cfg)
    if provider == "anthropic_api":
        return _summarize_api(system, user, cfg)
    return None


def summarize(work_signal: str, date_iso: str, cfg: SummarizerConfig,
              availability: str = "") -> str | None:
    if _resolve_provider(cfg.provider) == "none":
        log.info("요약기: 사용 안 함 (수집 데이터만 정리)")
        return None
    prompt = USER_TEMPLATE_KO.format(
        date=date_iso, signal=work_signal,
        availability=availability or "가용 데이터: (표기 없음)",
    )
    return _call(SYSTEM_KO, prompt, cfg)


# render_session_section() 가 붙이는 세션 질답 섹션의 머리글(파싱 기준).
SESSION_SECTION_HEADER = "## Claude Code 세션 (질답 흐름)"


def summarize_day(signal: str, date_iso: str, cfg: SummarizerConfig,
                  availability: str = "") -> str | None:
    """하루 업무일지 생성. 신호가 크면(세션 질답이 많으면) map-reduce 로 안전 처리.

    가벼우면 신호 그대로 단일 호출. 크면 세션 질답 섹션을 세션별로 쪼개 먼저 개별
    요약(병렬)한 뒤, 압축본으로 신호를 재구성해 종합한다. (컨텍스트 초과·품질 희석 방지)
    """
    if _resolve_provider(cfg.provider) == "none":
        log.info("요약기: 사용 안 함 (수집 데이터만 정리)")
        return None
    if len(signal) <= cfg.map_reduce_chars:
        return summarize(signal, date_iso, cfg, availability)

    marker = "\n" + SESSION_SECTION_HEADER
    if marker not in signal:
        return summarize(signal, date_iso, cfg, availability)   # 쪼갤 세션 섹션이 없음

    frame, sess = signal.split(marker, 1)
    blocks: list[tuple[str, str]] = []
    for i, chunk in enumerate(sess.strip().split("\n\n### ")):
        chunk = chunk.strip()
        if not chunk:
            continue
        if i > 0:
            chunk = "### " + chunk
        label = chunk.split("\n", 1)[0].lstrip("# ").strip()
        blocks.append((label, chunk))
    if not blocks:
        return summarize(signal, date_iso, cfg, availability)

    log.info("신호 %d자(>%d) → map-reduce: 세션 %d개 개별 요약(병렬 %d) 후 종합",
             len(signal), cfg.map_reduce_chars, len(blocks), cfg.map_workers)
    condensed = _map_condense(blocks, cfg)
    sess_md = "\n\n".join(f"### {lbl}\n{summ}" for lbl, summ in condensed if summ)
    new_signal = frame.rstrip() + "\n\n## Claude Code 세션 요약\n" + sess_md
    if len(new_signal) > cfg.map_reduce_chars:
        # 작은 세션이 많아 통과분만으로도 여전히 크면 전부 강제 압축(reduce 입력 폭주 방지).
        condensed = _map_condense(blocks, cfg, force_all=True)
        sess_md = "\n\n".join(f"### {lbl}\n{summ}" for lbl, summ in condensed if summ)
        new_signal = frame.rstrip() + "\n\n## Claude Code 세션 요약\n" + sess_md
    return summarize(new_signal, date_iso, cfg, availability)


def _block_body(block: str) -> str:
    """'### 헤더\\n<본문>' 에서 헤더 줄을 떼고 본문만."""
    parts = block.split("\n", 1)
    return (parts[1] if len(parts) > 1 else "").strip()


def _map_condense(blocks, cfg: SummarizerConfig, force_all: bool = False):
    """세션 블록들을 병렬로 개별 압축. force_all 이 아니면 작은 세션은 LLM 없이 원문 유지하고
    큰 세션만 압축(임계 초과면 조각내 2단). force_all 이면 전부 압축(총량이 여전히 클 때).
    반환: [(라벨, 본문 요약), ...]."""
    from concurrent.futures import ThreadPoolExecutor

    small = max(600, cfg.map_reduce_chars // 12)   # 이보다 작은 세션은 질답 원문 그대로

    def condense_one(item):
        lbl, block = item
        if not force_all and len(block) <= small:
            return (lbl, _block_body(block))       # 작은 세션: 압축 없이 원문(호출 절약)
        big = block
        if len(big) > cfg.map_reduce_chars:
            big = _condense_chunks(lbl, big, cfg)
        summ = _call(CONDENSE_SYSTEM_KO, big, cfg)
        # 최종 압축 실패 시엔 (원문 block 이 아니라) 이미 만든 조각요약 big 으로 폴백.
        return (lbl, (summ or _block_body(big)).strip())

    if not blocks:
        return []
    workers = max(1, min(cfg.map_workers, len(blocks)))
    with ThreadPoolExecutor(max_workers=workers) as ex:
        return list(ex.map(condense_one, blocks))


def _condense_chunks(label: str, block: str, cfg: SummarizerConfig) -> str:
    """초대형 세션 블록을 줄 단위로 조각내 각 조각을 먼저 요약, 이어붙인다(세션 내부 map)."""
    lines = block.split("\n")
    header = lines[0] if lines else label
    chunks: list[str] = []
    cur: list[str] = []
    size = 0
    for ln in lines[1:]:
        if size + len(ln) > cfg.map_reduce_chars and cur:
            chunks.append("\n".join(cur))
            cur, size = [], 0
        cur.append(ln)
        size += len(ln) + 1
    if cur:
        chunks.append("\n".join(cur))
    parts: list[str] = []
    for i, ch in enumerate(chunks, 1):
        summ = _call(CONDENSE_SYSTEM_KO, f"{header}\n(파트 {i}/{len(chunks)})\n{ch}", cfg)
        if summ:
            parts.append(summ.strip())
    return header + "\n" + "\n".join(parts)


_KNOWN_PROVIDERS = {"none", "claude_cli", "anthropic_api"}


def _resolve_provider(provider: str) -> str:
    if provider == "auto":
        if _claude_exe():
            return "claude_cli"
        if _anthropic_available():
            return "anthropic_api"
        log.warning("요약기: claude CLI 도 Anthropic SDK 도 없어 요약을 건너뜁니다.")
        return "none"
    if provider not in _KNOWN_PROVIDERS:
        log.warning(
            "알 수 없는 summarizer.provider %r → 요약을 건너뜁니다. "
            "(auto | none | claude_cli | anthropic_api)",
            provider,
        )
        return "none"
    return provider


def _summarize_cli(system: str, prompt: str, cfg: SummarizerConfig) -> str | None:
    from .render import WORKLOG_SENTINEL

    exe = _claude_exe()
    if not exe:
        log.warning("claude CLI 를 찾을 수 없어 요약을 건너뜁니다.")
        return None
    # 이 요약 호출이 만드는 Claude 세션을 나중에 확실히 걸러내기 위한 표식(맨 앞).
    full = WORKLOG_SENTINEL + "\n" + system + "\n\n" + prompt
    try:
        proc = subprocess.run(
            [exe, "-p", "--model", cfg.model],
            input=full,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=240,
            **no_window_kwargs(),
        )
    except (subprocess.TimeoutExpired, OSError) as e:
        log.warning("claude CLI 요약 실패: %s", e)
        return None
    if proc.returncode != 0:
        log.warning("claude CLI 요약 실패(exit %s): %s", proc.returncode, (proc.stderr or "")[:300])
        return None
    out = (proc.stdout or "").strip()
    return out or None


def _summarize_api(system: str, prompt: str, cfg: SummarizerConfig) -> str | None:
    try:
        import anthropic
    except ImportError:
        log.warning('anthropic SDK 미설치. `pip install "worklog-generator[llm]"` 또는 provider 를 claude_cli 로.')
        return None
    try:
        client = anthropic.Anthropic()
        resp = client.messages.create(
            model=cfg.model,
            max_tokens=cfg.max_tokens,
            system=system,
            messages=[{"role": "user", "content": prompt}],
        )
    except Exception as e:  # noqa: BLE001  (인증/네트워크/모델 오류 모두 방어)
        log.warning("Anthropic API 요약 실패: %s", e)
        return None
    parts = [b.text for b in resp.content if getattr(b, "type", None) == "text"]
    out = "".join(parts).strip()
    return out or None


def _anthropic_available() -> bool:
    import importlib.util

    return importlib.util.find_spec("anthropic") is not None
