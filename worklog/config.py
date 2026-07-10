"""설정 로딩.

우선순위: 코드 기본값  <  config.yaml  <  .env(비밀 값).
비밀 값(토큰/키/서비스계정)은 .env(환경변수)에서만 읽는다.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None  # type: ignore

try:
    from dotenv import load_dotenv
except ImportError:  # pragma: no cover
    def load_dotenv(*_a, **_k):  # type: ignore
        return False


# --------------------------------------------------------------------------- #
# 설정 dataclass
# --------------------------------------------------------------------------- #


@dataclass
class GitConfig:
    enabled: bool = True
    repos: list[str] = field(default_factory=list)
    scan_roots: list[str] = field(default_factory=list)
    scan_all_drives: bool = False   # True 면 scan_roots 대신 모든 고정 디스크를 스캔
    scan_depth: int = 5
    # 비우면 저장소 git 사용자(user.email 핸들)로 '내 커밋만' 자동 필터. 값 지정 시 그걸로 --author.
    author: str = ""
    authors: list[str] = field(default_factory=list)   # 추가 '내 신원'(이메일/핸들) — 자동감지와 OR 매칭
    include_claude_cwds: bool = True


@dataclass
class ClaudeConfig:
    enabled: bool = True
    projects_dir: str = ""   # 빈 값이면 ~/.claude/projects
    include_read: bool = False
    max_intent_len: int = 300
    max_qa_turns: int = 120       # 세션당 수집할 질답 상한(초과분은 생략 표기; 볼륨은 map-reduce가 처리)
    max_answer_len: int = 180     # 질답의 '답' 요지 최대 길이


@dataclass
class CodexConfig:
    enabled: bool = True
    sessions_dir: str = ""        # 빈 값이면 ${CODEX_HOME|~/.codex}/sessions
    include_read: bool = False
    max_intent_len: int = 300
    max_qa_turns: int = 120
    max_answer_len: int = 180
    max_lines: int = 200_000      # 초대형 롤아웃 파일 스트리밍 상한


@dataclass
class NaverWorksConfig:
    enabled: bool = False
    user_id: str = ""
    calendar_id: str = ""                # (구버전 단일) 하위호환용
    calendar_ids: list[str] = field(default_factory=list)  # 다중 선택
    scope: str = "calendar.read"
    # 아래는 .env 에서 채워짐
    client_id: str = ""
    client_secret: str = ""
    service_account: str = ""
    private_key: str = ""        # PEM 내용
    private_key_path: str = ""


@dataclass
class SummarizerConfig:
    provider: str = "auto"       # auto | claude_cli | anthropic_api | none
    model: str = "claude-opus-4-8"
    language: str = "ko"
    max_tokens: int = 4000
    # 세션 질답 총량이 이 글자수를 넘으면 단일 호출 대신 map-reduce(세션별 요약→종합)로.
    map_reduce_chars: int = 20000
    map_workers: int = 4         # 세션별 요약 병렬 수(claude CLI 동시 호출 상한)


def documents_dir() -> str:
    """'현재 로그인한 사용자'의 문서(Documents) 폴더. (사용자명 하드코딩 없음)

    Windows: ① 공식 Known Folder API(SHGetKnownFolderPath, 현재 사용자 토큰·OneDrive 대응)
             ② 레지스트리 HKCU  ③ 현재 사용자 홈(~)/Documents — 모두 현재 사용자로 해석.
    그 외 OS: ~/Documents.
    """
    if os.name == "nt":
        for resolver in (_win_known_documents, _win_registry_documents):
            p = resolver()
            if p:
                return p
    return os.path.join(os.path.expanduser("~"), "Documents")


def _win_known_documents() -> str | None:
    """SHGetKnownFolderPath(FOLDERID_Documents) — 현재 사용자의 문서 폴더."""
    try:
        import ctypes
        from ctypes import wintypes

        class _GUID(ctypes.Structure):
            _fields_ = [
                ("Data1", wintypes.DWORD), ("Data2", wintypes.WORD),
                ("Data3", wintypes.WORD), ("Data4", ctypes.c_byte * 8),
            ]

        # FOLDERID_Documents = {FDD39AD0-238F-46AF-ADB4-6C85480369C7}
        fid = _GUID(0xFDD39AD0, 0x238F, 0x46AF,
                    (ctypes.c_byte * 8)(0xAD, 0xB4, 0x6C, 0x85, 0x48, 0x03, 0x69, 0xC7))
        ptr = ctypes.c_wchar_p()
        hr = ctypes.windll.shell32.SHGetKnownFolderPath(
            ctypes.byref(fid), 0, None, ctypes.byref(ptr))
        if hr != 0 or not ptr.value:
            return None
        path = ptr.value
        ctypes.windll.ole32.CoTaskMemFree(ptr)
        return path
    except Exception:  # noqa: BLE001
        return None


def _win_registry_documents() -> str | None:
    try:
        import winreg
        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders",
        ) as k:
            val, _ = winreg.QueryValueEx(k, "Personal")
            p = os.path.expandvars(val)   # %USERPROFILE% 등 → 현재 사용자로 확장
            return p or None
    except OSError:
        return None


@dataclass
class MarkdownOutputConfig:
    enabled: bool = True
    # 기본 저장 위치: 문서 폴더의 '업무일지' 하위. config.yaml 로 덮어쓸 수 있음.
    dir: str = field(default_factory=lambda: os.path.join(documents_dir(), "업무일지"))


@dataclass
class ObsidianOutputConfig:
    enabled: bool = False
    vault_dir: str = ""
    subdir: str = "업무일지"


@dataclass
class NotionOutputConfig:
    enabled: bool = False
    parent_type: str = "page"    # page | database
    parent_id: str = ""
    title_prop: str = "Name"
    version: str = "2022-06-28"   # 공식 안정 Notion-Version (미검증 미래 날짜는 400 유발)
    token: str = ""              # .env: NOTION_TOKEN


@dataclass
class OutputsConfig:
    markdown: MarkdownOutputConfig = field(default_factory=MarkdownOutputConfig)
    obsidian: ObsidianOutputConfig = field(default_factory=ObsidianOutputConfig)
    notion: NotionOutputConfig = field(default_factory=NotionOutputConfig)


@dataclass
class Config:
    timezone: str = "Asia/Seoul"
    include_raw_data: bool = False   # 저장 파일에 '수집 데이터 원본' 부록 포함 여부
    git: GitConfig = field(default_factory=GitConfig)
    claude: ClaudeConfig = field(default_factory=ClaudeConfig)
    codex: CodexConfig = field(default_factory=CodexConfig)
    naverworks: NaverWorksConfig = field(default_factory=NaverWorksConfig)
    summarizer: SummarizerConfig = field(default_factory=SummarizerConfig)
    outputs: OutputsConfig = field(default_factory=OutputsConfig)


# --------------------------------------------------------------------------- #
# 로딩
# --------------------------------------------------------------------------- #


def _apply(target, data: dict | None) -> None:
    """dict 의 키를 dataclass 인스턴스 필드에 얕게 덮어쓴다(존재하는 키만)."""
    if not data:
        return
    for key, value in data.items():
        if hasattr(target, key) and value is not None:
            setattr(target, key, value)


def _load_yaml(path: Path) -> dict:
    if not path.exists():
        return {}
    if yaml is None:
        raise RuntimeError("PyYAML 이 설치되어 있지 않습니다. `pip install pyyaml`")
    with path.open(encoding="utf-8") as f:
        return yaml.safe_load(f) or {}


def load_config(config_path: str | None = None) -> Config:
    """config.yaml + .env 를 읽어 Config 를 만든다."""
    # .env 로드 (cwd 기준)
    load_dotenv()

    cfg = Config()

    # config.yaml 위치 결정
    path = Path(config_path) if config_path else Path("config.yaml")
    raw = _load_yaml(path)

    _apply(cfg, {"timezone": raw.get("timezone"), "include_raw_data": raw.get("include_raw_data")})

    sources = raw.get("sources") or {}
    _apply(cfg.git, sources.get("git"))
    _apply(cfg.claude, sources.get("claude"))
    _apply(cfg.codex, sources.get("codex"))
    _apply(cfg.naverworks, sources.get("naverworks"))
    _apply(cfg.summarizer, raw.get("summarizer"))

    outputs = raw.get("outputs") or {}
    _apply(cfg.outputs.markdown, outputs.get("markdown"))
    _apply(cfg.outputs.obsidian, outputs.get("obsidian"))
    _apply(cfg.outputs.notion, outputs.get("notion"))

    # --- 비밀 값(환경변수) 오버레이 ---
    _overlay_secrets(cfg)

    # --- 앱에서 저장한 설정 오버레이 (최우선: 앱에서 명시적으로 넣은 값) ---
    _apply_app_settings(cfg, load_app_settings())
    return cfg


def _overlay_secrets(cfg: Config) -> None:
    # 환경변수가 '비어있지 않을 때만' 덮어쓴다(export 만 하고 빈 값이면 기존 값 유지).
    def env(key, current):
        v = os.environ.get(key)
        return v if v else current

    nw = cfg.naverworks
    nw.client_id = env("NAVERWORKS_CLIENT_ID", nw.client_id)
    nw.client_secret = env("NAVERWORKS_CLIENT_SECRET", nw.client_secret)
    nw.service_account = env("NAVERWORKS_SERVICE_ACCOUNT", nw.service_account)
    nw.private_key = env("NAVERWORKS_PRIVATE_KEY", nw.private_key)
    nw.private_key_path = env("NAVERWORKS_PRIVATE_KEY_PATH", nw.private_key_path)
    nw.user_id = env("NAVERWORKS_USER_ID", nw.user_id)
    nw.calendar_id = env("NAVERWORKS_CALENDAR_ID", nw.calendar_id)

    cfg.outputs.notion.token = env("NOTION_TOKEN", cfg.outputs.notion.token)


# --------------------------------------------------------------------------- #
# 앱 설정 저장소 (~/.worklog/settings.json) — 앱 UI 에서 저장/불러오기
# config.yaml/.env 위에 오버레이된다. 비밀 값도 여기 저장(로컬 전용 파일).
# --------------------------------------------------------------------------- #


def app_settings_path() -> Path:
    override = os.environ.get("WORKLOG_SETTINGS")
    if override:
        return Path(override)
    return Path.home() / ".worklog" / "settings.json"


def load_app_settings() -> dict:
    path = app_settings_path()
    try:
        with path.open(encoding="utf-8") as f:
            data = json.load(f)
        return data if isinstance(data, dict) else {}
    except json.JSONDecodeError:
        # 손상된 파일(부분 쓰기 등)은 .bak 로 보존해 조용한 전소실을 막는다.
        try:
            path.replace(path.with_suffix(".json.bak"))
        except OSError:
            pass
        return {}
    except OSError:
        return {}


def save_app_settings(data: dict) -> Path:
    path = app_settings_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    # 임시 파일에 완전히 쓴 뒤 os.replace 로 원자적 교체 — 크래시/디스크풀 시에도
    # 기존 settings.json 이 절반만 쓰여 손상되지 않는다(비밀값 전소실 방지).
    tmp = path.with_suffix(".json.tmp")
    with tmp.open("w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
    os.replace(tmp, path)
    return path


def _apply_nonempty(target, data: dict | None) -> None:
    """앱 설정 오버레이 전용: None/빈 문자열은 '미설정'으로 보고 건너뛴다.

    사용자가 UI 에서 비워둔 칸(예: NaverWorks 자격증명)이 .env 로 채워진 값을
    덮어써 지우는 것을 방지한다. (bool False, 숫자 0 등은 정상 적용)
    """
    if not data:
        return
    for key, value in data.items():
        if not hasattr(target, key):
            continue
        if value is None or value == "":
            continue
        setattr(target, key, value)


def _apply_app_settings(cfg: Config, store: dict) -> None:
    if not store:
        return
    if store.get("timezone"):
        cfg.timezone = store["timezone"]
    if "include_raw_data" in store:
        cfg.include_raw_data = bool(store["include_raw_data"])
    # 비밀/자격증명이 없는 섹션은 정상 덮어쓰기(_apply) → 앱에서 값을 '지우기'도 반영됨.
    # 자격증명이 .env 로도 들어오는 섹션(naverworks, notion)만 빈값-유지(_apply_nonempty)로
    # .env 값이 빈 입력에 지워지는 것을 막는다.
    _apply(cfg.summarizer, store.get("summarizer"))

    sources = store.get("sources") or {}
    _apply(cfg.git, sources.get("git"))
    _apply(cfg.claude, sources.get("claude"))
    _apply(cfg.codex, sources.get("codex"))
    _apply_nonempty(cfg.naverworks, sources.get("naverworks"))

    outputs = store.get("outputs") or {}
    _apply(cfg.outputs.markdown, outputs.get("markdown"))
    _apply(cfg.outputs.obsidian, outputs.get("obsidian"))
    _apply_nonempty(cfg.outputs.notion, outputs.get("notion"))


# --------------------------------------------------------------------------- #
# `worklog --init` 이 생성하는 템플릿
# --------------------------------------------------------------------------- #

# 패키지 내부 templates/ 에서 읽는다(wheel 설치 시에도 포함되도록 force-include).
_TEMPLATES = Path(__file__).resolve().parent / "templates"
EXAMPLE_CONFIG_PATH = _TEMPLATES / "config.example.yaml"
EXAMPLE_ENV_PATH = _TEMPLATES / "env.example"
