"""LLM 종합(요약)기.

수집한 '사실 Markdown' 을 Claude 에게 주고 자연어 업무일지로 다듬는다.
provider:
  auto         → claude CLI 있으면 사용, 없으면 Anthropic API, 둘 다 없으면 None
  claude_cli   → 설치된 `claude` CLI (별도 API 키 불필요)
  anthropic_api→ Anthropic SDK (ANTHROPIC_API_KEY 또는 ant 프로필)
  none         → 요약 생략
"""

from __future__ import annotations

import logging
import shutil
import subprocess

from .config import SummarizerConfig
from .util import no_window_kwargs

log = logging.getLogger("worklog")


SYSTEM_KO = (
    "너는 하루치 개발 활동 데이터를 '업무일지'로 문서화하는 도구다. "
    "목표 — '언제 무슨 일을 했는가'를 **시간 흐름대로** 문서화하고, 프로젝트별로도 정리하는 것.\n\n"
    "절대 규칙:\n"
    "1. 과정·시도·탐색·조사·대화 서술 금지. 오직 '완료된 결과물/변경'만. "
    "('~를 확인했다', '~를 살펴봤다', '~하려고 했다' 같은 과정 서술 금지)\n"
    "2. 각 항목은 한 줄, 개조식. 명사구나 완료형으로 끝내라. "
    "(예: 'KMS 검색 노드 V2 추가', '등록 버튼 활성화 버그 수정')\n"
    "3. git 커밋을 1차 근거로. 원본 프롬프트·명령어·파일 목록을 그대로 나열하지 마라.\n"
    "4. 데이터에 없는 것은 지어내지 마라. 제공되지 않은 소스의 섹션은 아예 만들지 마라.\n"
    "5. 미사여구·불필요한 이모지·장황체 금지. 짧고 담백하게.\n\n"
    "출력 구조:\n"
    "## 한 줄 요약 — 오늘을 한 문장으로.\n"
    "## 🕘 시간대별 업무 — 입력의 '시간순 이벤트'를 자연스러운 시간 블록"
    "(예: 오전/점심/오후·저녁, 또는 1~3시간 단위)으로 묶어 '몇 시경 무엇을 했다'를 문서화한다. "
    "각 블록은 **굵은 시간대**(예: **09–12시**) 아래 개조식 결과. "
    "연속·중복 작업은 하나로 합치고, **회의(📅)는 반드시 해당 시각에 명시**한다.\n"
    "## 오늘 한 일 — 같은 내용을 프로젝트별로 재정리(굵은 프로젝트명 + 개조식). 시간대별과 문구가 겹치면 짧게.\n"
    "## 시간 사용 — 시간추적 데이터가 있을 때만 1줄.\n\n"
    "전체가 한눈에 들어오게. 문단 쓰지 말고 불릿만 써라."
)

USER_TEMPLATE_KO = (
    "{availability}\n\n"
    "아래는 {date} 활동 데이터(이미 정제된 요약 신호 + 시간순 이벤트)다. 위 규칙대로 "
    "'시간대별 업무'와 '프로젝트별 정리'를 문서화한 업무일지 본문만 출력하라. 데이터 원문을 그대로 나열하지 마라.\n\n"
    "---\n{signal}\n---\n"
)


def summarize(work_signal: str, date_iso: str, cfg: SummarizerConfig,
              availability: str = "") -> str | None:
    provider = _resolve_provider(cfg.provider)
    if provider == "none":
        log.info("요약기: 사용 안 함 (수집 데이터만 정리)")
        return None

    prompt = USER_TEMPLATE_KO.format(
        date=date_iso, signal=work_signal,
        availability=availability or "가용 데이터: (표기 없음)",
    )

    if provider == "claude_cli":
        return _summarize_cli(prompt, cfg)
    if provider == "anthropic_api":
        return _summarize_api(prompt, cfg)
    return None


_KNOWN_PROVIDERS = {"none", "claude_cli", "anthropic_api"}


def _resolve_provider(provider: str) -> str:
    if provider == "auto":
        if shutil.which("claude"):
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


def _summarize_cli(prompt: str, cfg: SummarizerConfig) -> str | None:
    from .render import WORKLOG_SENTINEL

    exe = shutil.which("claude")
    if not exe:
        log.warning("claude CLI 를 찾을 수 없어 요약을 건너뜁니다.")
        return None
    # 이 요약 호출이 만드는 Claude 세션을 나중에 확실히 걸러내기 위한 표식(맨 앞).
    full = WORKLOG_SENTINEL + "\n" + SYSTEM_KO + "\n\n" + prompt
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


def _summarize_api(prompt: str, cfg: SummarizerConfig) -> str | None:
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
            system=SYSTEM_KO,
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
