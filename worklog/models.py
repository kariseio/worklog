"""수집된 데이터와 최종 업무일지를 담는 데이터 모델.

각 수집기(collector)는 여기 정의된 타입을 채워서 반환하고,
render/summarize/outputs 는 이 타입만 알면 된다. (수집기 구현과 분리)
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date, datetime

# --------------------------------------------------------------------------- #
# Git
# --------------------------------------------------------------------------- #


@dataclass
class GitCommit:
    repo: str
    hash: str
    author: str
    when: datetime
    subject: str
    files_changed: int = 0
    insertions: int = 0
    deletions: int = 0
    repo_path: str = ""   # 물리적 저장소 식별 키(git-common-dir). 동명이repo 구분용.

    @property
    def short_hash(self) -> str:
        return self.hash[:8]


@dataclass
class GitData:
    commits: list[GitCommit] = field(default_factory=list)

    @property
    def repos(self) -> list[str]:
        seen: list[str] = []
        for c in self.commits:
            if c.repo not in seen:
                seen.append(c.repo)
        return seen

    def commits_for(self, repo: str) -> list[GitCommit]:
        return [c for c in self.commits if c.repo == repo]


# --------------------------------------------------------------------------- #
# Claude Code 세션 로그
# --------------------------------------------------------------------------- #


@dataclass
class QATurn:
    """세션 안의 한 '질답' — 사용자 질문 + 그에 대한 어시스턴트 응답 요지."""

    time: str          # 로컬 시:분 (예: "14:03")
    question: str      # 사용자 프롬프트(정제·절삭)
    answer: str = ""   # 어시스턴트 응답 요지(프로즈 앞부분, 절삭)


@dataclass
class ClaudeSession:
    session_id: str | None
    project: str | None            # cwd 의 basename (표시용)
    cwd: str | None                # 실제 프로젝트 절대경로
    git_branch: str | None
    title: str | None              # ai-title (세션 요약 한 줄)
    intent: str | None             # 그날 첫 사용자 프롬프트
    files_edited: list[str] = field(default_factory=list)
    files_read: list[str] = field(default_factory=list)
    commands: list[str] = field(default_factory=list)
    tool_counts: dict[str, int] = field(default_factory=dict)
    output_tokens: int = 0
    first_ts: datetime | None = None
    last_ts: datetime | None = None
    qa: list[QATurn] = field(default_factory=list)   # 세션 내 질답 흐름(시간순)
    qa_dropped: int = 0                              # 상한 초과로 생략된 질답 수


@dataclass
class ClaudeData:
    sessions: list[ClaudeSession] = field(default_factory=list)

    @property
    def total_sessions(self) -> int:
        return len(self.sessions)

    @property
    def cwds(self) -> list[str]:
        seen: list[str] = []
        for s in self.sessions:
            if s.cwd and s.cwd not in seen:
                seen.append(s.cwd)
        return seen


# --------------------------------------------------------------------------- #
# NaverWorks 캘린더
# --------------------------------------------------------------------------- #


@dataclass
class CalendarEvent:
    title: str | None
    start: str | None              # ISO 문자열 또는 all-day 날짜
    end: str | None
    all_day: bool = False
    location: str | None = None
    description: str | None = None
    attendees: list[str] = field(default_factory=list)


@dataclass
class CalendarData:
    events: list[CalendarEvent] = field(default_factory=list)


# --------------------------------------------------------------------------- #
# 하루치 종합 + 최종 산출물
# --------------------------------------------------------------------------- #


@dataclass
class DailyData:
    target_date: date
    tz_name: str
    git: GitData | None = None
    claude: ClaudeData | None = None
    calendar: CalendarData | None = None
    warnings: list[str] = field(default_factory=list)

    def is_empty(self) -> bool:
        """LLM 요약을 돌릴 만한 실제 데이터가 하나라도 있는지."""
        if self.calendar and self.calendar.events:
            return False
        if self.git and self.git.commits:
            return False
        if self.claude and self.claude.sessions:
            return False
        return True


@dataclass
class WorkLog:
    target_date: date
    facts_markdown: str                 # 수집 데이터를 결정론적으로 정리한 부분
    full_markdown: str                  # 최종 문서(요약 + facts appendix)
    data: DailyData
    summary_markdown: str | None = None  # LLM 이 만든 자연어 요약 (없을 수 있음)
