"""Notion 출력: 새 페이지를 만들어 업무일지를 기록한다.

parent_type=page   → 부모 페이지의 하위 페이지로 생성
parent_type=database → 데이터베이스의 한 행(row)으로 생성 (title_prop 에 제목)

주의: 통합(Integration)을 대상 페이지/DB 에 '연결(Connections)' 해두지 않으면 404.
      children 는 요청당 최대 100블록이라 배치로 나눠 append 한다.
참고: developers.notion.com (post-page, patch-block-children)
"""

from __future__ import annotations

import re

from ..config import NotionOutputConfig
from ..models import WorkLog
from .base import Sink, SinkResult

try:
    import requests
except ImportError:  # pragma: no cover
    requests = None  # type: ignore

API = "https://api.notion.com/v1"
MAX_BLOCKS = 100
MAX_RICH_TEXT = 1900  # 한 text 오브젝트는 2000자 제한 → 여유 두고 자름


def test_connection(cfg: NotionOutputConfig) -> tuple[bool, str]:
    """실제 Notion API 로 토큰 + 대상(page/database) 접근을 확인."""
    if requests is None:
        return False, "requests 미설치"
    if not cfg.token:
        return False, "Notion 토큰을 입력하세요."
    if not cfg.parent_id:
        return False, "대상 page_id / database_id 를 입력하세요."
    headers = {
        "Authorization": f"Bearer {cfg.token}",
        "Notion-Version": cfg.version,
    }
    kind = "databases" if cfg.parent_type == "database" else "pages"
    url = f"{API}/{kind}/{cfg.parent_id}"
    try:
        resp = requests.get(url, headers=headers, timeout=15)
    except requests.exceptions.RequestException as e:
        return False, f"네트워크 오류: {e}"

    if resp.status_code == 200:
        try:
            data = resp.json()
        except ValueError:
            return False, f"예상치 못한 응답 (HTTP 200, JSON 아님): {resp.text[:200]}"
        title = _extract_title(data) or "(제목 없음)"
        return True, f"연결됨 · {'DB' if kind == 'databases' else '페이지'} '{title}'"
    if resp.status_code == 401:
        return False, "토큰이 유효하지 않습니다 (401)."
    if resp.status_code == 404:
        return False, "대상을 찾을 수 없습니다. 통합을 페이지/DB 에 '연결(Connections)' 했는지 확인하세요 (404)."
    return False, f"실패 (HTTP {resp.status_code}): {resp.text[:200]}"


def _extract_title(obj: dict) -> str | None:
    # database: obj["title"] 는 rich_text 배열. page: properties 안의 title 타입.
    title = obj.get("title")
    if isinstance(title, list) and title:
        return "".join(t.get("plain_text", "") for t in title) or None
    props = obj.get("properties") or {}
    for prop in props.values():
        if isinstance(prop, dict) and prop.get("type") == "title":
            arr = prop.get("title") or []
            return "".join(t.get("plain_text", "") for t in arr) or None
    return None


class NotionSink(Sink):
    name = "notion"

    def __init__(self, cfg: NotionOutputConfig):
        self.cfg = cfg

    def _headers(self) -> dict:
        return {
            "Authorization": f"Bearer {self.cfg.token}",
            "Notion-Version": self.cfg.version,
            "Content-Type": "application/json",
        }

    def write(self, worklog: WorkLog) -> SinkResult:
        if requests is None:
            return SinkResult.failure(self.name, "requests 미설치")
        if not self.cfg.token:
            return SinkResult.failure(self.name, "NOTION_TOKEN 미설정")
        if not self.cfg.parent_id:
            return SinkResult.failure(self.name, "outputs.notion.parent_id 미설정")

        title = f"업무일지 {worklog.target_date.isoformat()}"
        blocks = markdown_to_blocks(worklog.full_markdown)

        try:
            page = self._create_page(title, blocks[:MAX_BLOCKS])
        except _NotionError as e:
            return SinkResult.failure(self.name, str(e))

        page_id = page.get("id")
        # 100블록 초과분은 append
        remaining = blocks[MAX_BLOCKS:]
        for i in range(0, len(remaining), MAX_BLOCKS):
            try:
                self._append(page_id, remaining[i : i + MAX_BLOCKS])
            except _NotionError as e:
                return SinkResult.failure(self.name, f"블록 추가 실패: {e}")

        return SinkResult.success(self.name, page.get("url") or page_id or "(created)")

    def _create_page(self, title: str, blocks: list) -> dict:
        if self.cfg.parent_type == "database":
            body = {
                "parent": {"database_id": self.cfg.parent_id},
                "properties": {self.cfg.title_prop: {"title": _rt(title)}},
                "children": blocks,
            }
        else:
            body = {
                "parent": {"page_id": self.cfg.parent_id},
                "properties": {"title": {"title": _rt(title)}},
                "children": blocks,
            }
        return self._post(f"{API}/pages", body)

    def _append(self, block_id: str, blocks: list) -> dict:
        return self._patch(f"{API}/blocks/{block_id}/children", {"children": blocks})

    def _post(self, url: str, body: dict) -> dict:
        return self._request("post", url, body)

    def _patch(self, url: str, body: dict) -> dict:
        return self._request("patch", url, body)

    def _request(self, method: str, url: str, body: dict) -> dict:
        try:
            resp = requests.request(method, url, headers=self._headers(), json=body, timeout=30)
        except requests.exceptions.RequestException as e:
            raise _NotionError(str(e)) from e
        if resp.status_code >= 300:
            raise _NotionError(f"HTTP {resp.status_code}: {resp.text[:400]}")
        return resp.json()


# --------------------------------------------------------------------------- #
# Markdown → Notion 블록 (간단 변환기)
# --------------------------------------------------------------------------- #


def markdown_to_blocks(md: str) -> list:
    blocks: list = []
    in_code = False
    code_lines: list[str] = []
    code_lang = "plain text"

    for raw in md.splitlines():
        line = raw.rstrip("\n")

        # 코드펜스
        fence = line.strip()
        if fence.startswith("```"):
            if not in_code:
                in_code = True
                code_lines = []
                lang = fence[3:].strip().lower()
                code_lang = _notion_lang(lang)
            else:
                blocks.append(_code_block("\n".join(code_lines), code_lang))
                in_code = False
            continue
        if in_code:
            code_lines.append(line)
            continue

        stripped = line.strip()
        if not stripped:
            continue

        if stripped.startswith("### "):
            blocks.append(_heading(stripped[4:], 3))
        elif stripped.startswith("## "):
            blocks.append(_heading(stripped[3:], 2))
        elif stripped.startswith("# "):
            blocks.append(_heading(stripped[2:], 1))
        elif re.fullmatch(r"-{3,}", stripped):   # 하이픈으로만 된 줄만 divider
            blocks.append({"object": "block", "type": "divider", "divider": {}})
        elif re.match(r"^[-*] ", stripped) or re.match(r"^\s+[-*] ", line):
            # 들여쓰기 bullet 도 평면 bullet 으로 (2단계 이상 중첩은 단순화)
            text = re.sub(r"^\s*[-*] ", "", line)
            blocks.append(_bullet(text))
        elif stripped in ("<details>", "</details>") or stripped.startswith("<summary"):
            continue   # 데이터 접기 HTML 래퍼는 Notion 에서 리터럴 텍스트로 새므로 버린다
        elif stripped.startswith("|") and stripped.endswith("|") and len(stripped) > 1:
            # GFM 표: 구분선(|---|)은 버리고, 데이터 행은 셀을 ' · ' 로 이어 가독 문단으로.
            cells = [c.strip() for c in stripped.strip("|").split("|")]
            nonempty = [c for c in cells if c]
            if nonempty and all(re.fullmatch(r":?-{2,}:?", c) for c in nonempty):
                continue
            blocks.append(_paragraph(" · ".join(nonempty)))
        else:
            blocks.append(_paragraph(stripped))

    if in_code:  # 닫히지 않은 코드펜스 방어
        blocks.append(_code_block("\n".join(code_lines), code_lang))

    return blocks


def _rt(text: str) -> list:
    """plain text → rich_text 배열. 2000자 제한을 넘으면 여러 조각으로 분할하고 인라인 마크다운은 제거."""
    return _rt_raw(_strip_inline(text))


def _rt_raw(text: str) -> list:
    """인라인 마크다운 제거 없이 2000자 분할만. (코드블록처럼 원문 그대로 넣어야 할 때)"""
    chunks = [text[i : i + MAX_RICH_TEXT] for i in range(0, len(text), MAX_RICH_TEXT)] or [""]
    return [{"type": "text", "text": {"content": c}} for c in chunks]


def _strip_inline(text: str) -> str:
    # **bold**, `code`, [label](url) 같은 인라인은 Notion 리치텍스트로 정확 변환하지 않고
    # 표기만 정리해 가독성 유지 (업무일지 용도에는 충분).
    text = re.sub(r"\*\*(.+?)\*\*", r"\1", text)
    text = re.sub(r"`(.+?)`", r"\1", text)
    text = re.sub(r"\[(.+?)\]\((.+?)\)", r"\1", text)
    return text


def _heading(text: str, level: int) -> dict:
    key = f"heading_{min(level, 3)}"
    return {"object": "block", "type": key, key: {"rich_text": _rt(text)}}


def _bullet(text: str) -> dict:
    return {
        "object": "block",
        "type": "bulleted_list_item",
        "bulleted_list_item": {"rich_text": _rt(text)},
    }


def _paragraph(text: str) -> dict:
    return {"object": "block", "type": "paragraph", "paragraph": {"rich_text": _rt(text)}}


def _code_block(text: str, lang: str) -> dict:
    return {
        "object": "block",
        "type": "code",
        "code": {"rich_text": _rt_raw(text), "language": lang},   # 코드 원문 보존
    }


_NOTION_LANGS = {
    "py": "python", "python": "python", "js": "javascript", "javascript": "javascript",
    "ts": "typescript", "typescript": "typescript", "bash": "bash", "sh": "shell",
    "shell": "shell", "json": "json", "yaml": "yaml", "yml": "yaml", "sql": "sql",
    "": "plain text",
}


def _notion_lang(lang: str) -> str:
    return _NOTION_LANGS.get(lang, "plain text")


class _NotionError(Exception):
    pass
