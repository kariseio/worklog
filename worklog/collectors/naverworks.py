"""NaverWorks(네이버웍스) 캘린더 수집기.

서비스 계정(JWT-bearer, RS256)으로 액세스 토큰을 발급받아
그날의 일정을 가져온다. 자격증명이 없으면 조용히 건너뛴다.

인증:  POST https://auth.worksmobile.com/oauth2/v2.0/token
       grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer
       (JWT claims: iss=client_id, sub=service_account, iat, exp<=+3600, RS256)
캘린더: GET https://www.worksapis.com/v1.0/users/{userId}/calendar/events
       ?fromDateTime=..&untilDateTime=..  (RFC3339, 31일 이내)
       응답: events[].eventComponents[] (summary/start/end/location/attendees ...)

참고: developers.worksmobile.com (auth-jwt, calendar-default-event-user-list)
"""

from __future__ import annotations

import time
from urllib.parse import quote

from ..config import NaverWorksConfig
from ..models import CalendarData, CalendarEvent
from .base import CollectContext, Collector, CollectorResult

try:
    import requests
except ImportError:  # pragma: no cover
    requests = None  # type: ignore

TOKEN_URL = "https://auth.worksmobile.com/oauth2/v2.0/token"
API_BASE = "https://www.worksapis.com/v1.0"


def test_connection(cfg: NaverWorksConfig) -> tuple[bool, str]:
    """실제로 액세스 토큰 발급을 시도해 자격증명이 유효한지 확인."""
    if requests is None:
        return False, "requests 미설치"
    missing = [
        n for n, v in [
            ("Client ID", cfg.client_id),
            ("Client Secret", cfg.client_secret),
            ("Service Account", cfg.service_account),
        ] if not v
    ]
    if not (cfg.private_key or cfg.private_key_path):
        missing.append("Private Key")
    if missing:
        return False, f"필요한 값이 비어있습니다: {', '.join(missing)}"

    collector = NaverWorksCollector(cfg)
    try:
        collector._get_token()
    except _NWError as e:
        return False, f"토큰 발급 실패: {e}"
    return True, "연결됨 · 액세스 토큰 발급 성공"


def list_calendars(cfg: NaverWorksConfig) -> tuple[bool, object]:
    """사용자의 캘린더 목록을 가져온다. 성공 시 [{calendar_id, name}], 실패 시 에러 메시지."""
    if requests is None:
        return False, "requests 미설치"
    if not cfg.user_id:
        return False, "사용자 ID 를 먼저 입력하세요."

    collector = NaverWorksCollector(cfg)
    try:
        token = collector._get_token()
    except _NWError as e:
        return False, f"토큰 발급 실패: {e}"

    # 사용자가 가진 캘린더 목록. /calendars 는 calendarIds 필수(배치조회)라 못 쓰고,
    # /calendar-personals 가 '내 캘린더들의 속성'을 돌려준다.
    url = f"{API_BASE}/users/{quote(cfg.user_id)}/calendar-personals"
    calendars: list[dict] = []
    cursor = None
    try:
        for _ in range(10):  # 페이지네이션 안전 상한
            params = {"cursor": cursor} if cursor else {}
            resp = requests.get(url, headers={"Authorization": f"Bearer {token}"},
                                params=params, timeout=20)
            if resp.status_code != 200:
                return False, f"HTTP {resp.status_code}: {resp.text[:200]}"
            try:
                data = resp.json()
            except ValueError:
                return False, "응답 파싱 실패(JSON 아님)"
            raw = _extract_list(data)
            for c in raw:
                if not isinstance(c, dict):
                    continue
                cid = c.get("calendarId") or c.get("id")
                name = (c.get("calendarName") or c.get("name") or c.get("summary")
                        or c.get("title") or c.get("subject") or "(이름 없음)")
                if cid:
                    calendars.append({"calendar_id": str(cid), "name": name})
            cursor = (data.get("responseMetaData") or {}).get("nextCursor") if isinstance(data, dict) else None
            if not cursor:
                break
    except requests.exceptions.RequestException as e:
        return False, str(e)
    return True, calendars


def _extract_list(data) -> list:
    """응답에서 캘린더 배열을 찾아낸다(키 이름이 문서마다 달라 방어적으로)."""
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        for key in ("calendarPersonals", "calendars", "calendarList", "list", "elements", "items"):
            if isinstance(data.get(key), list):
                return data[key]
        # 키를 모르면, dict 값 중 첫 번째 list 를 사용
        for v in data.values():
            if isinstance(v, list):
                return v
    return []


class NaverWorksCollector(Collector):
    name = "naverworks"

    def __init__(self, cfg: NaverWorksConfig):
        self.cfg = cfg

    def collect(self, ctx: CollectContext) -> CollectorResult:
        c = self.cfg
        if requests is None:
            return CollectorResult.fail(self.name, "requests 미설치")

        missing = [
            n for n, v in [
                ("NAVERWORKS_CLIENT_ID", c.client_id),
                ("NAVERWORKS_CLIENT_SECRET", c.client_secret),
                ("NAVERWORKS_SERVICE_ACCOUNT", c.service_account),
                ("user_id", c.user_id),
            ] if not v
        ]
        if not (c.private_key or c.private_key_path):
            missing.append("NAVERWORKS_PRIVATE_KEY(_PATH)")
        if missing:
            return CollectorResult.skip(
                self.name, f"NaverWorks 자격증명 미설정: {', '.join(missing)}"
            )

        try:
            token = self._get_token()
        except _NWError as e:
            return CollectorResult.fail(self.name, f"토큰 발급 실패: {e}")

        try:
            events = self._get_events(token, ctx)
        except _NWError as e:
            return CollectorResult.fail(self.name, f"캘린더 조회 실패: {e}")

        return CollectorResult(name=self.name, data=CalendarData(events=events))

    # ------------------------------------------------------------------ #

    def _private_key(self) -> str:
        c = self.cfg
        if c.private_key:
            return c.private_key
        try:
            with open(c.private_key_path, encoding="utf-8") as f:
                return f.read()
        except OSError as e:
            raise _NWError(f"private key 파일을 읽을 수 없습니다: {e}") from e

    def _build_assertion(self) -> str:
        try:
            import jwt  # PyJWT (RS256 → cryptography 필요)
        except ImportError as e:
            raise _NWError('PyJWT 미설치. `pip install "pyjwt[crypto]"`') from e

        now = int(time.time())
        payload = {
            "iss": self.cfg.client_id,        # issuer = Client ID
            "sub": self.cfg.service_account,  # subject = 서비스 계정
            "iat": now,
            "exp": now + 3600,                # 최대 60분
        }
        try:
            return jwt.encode(payload, self._private_key(), algorithm="RS256")
        except Exception as e:  # noqa: BLE001
            raise _NWError(f"JWT 서명 실패(키 형식 확인): {e}") from e

    def _get_token(self) -> str:
        data = {
            "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
            "assertion": self._build_assertion(),
            "client_id": self.cfg.client_id,
            "client_secret": self.cfg.client_secret,
            "scope": self.cfg.scope,
        }
        try:
            resp = requests.post(
                TOKEN_URL, data=data,
                headers={"Content-Type": "application/x-www-form-urlencoded; charset=UTF-8"},
                timeout=15,
            )
        except requests.exceptions.RequestException as e:
            raise _NWError(str(e)) from e
        if resp.status_code != 200:
            hint = ""
            if "invalid_scope" in resp.text:
                hint = (
                    f" ⟶ Developer Console 앱의 'OAuth Scope' 에 '{self.cfg.scope}' 가 "
                    "등록/승인돼 있는지 확인하세요. (읽기전용은 calendar.read, 읽기/쓰기는 calendar)"
                )
            raise _NWError(f"HTTP {resp.status_code}: {resp.text[:300]}{hint}")
        try:
            token = resp.json().get("access_token")
        except ValueError as e:
            raise _NWError(f"토큰 응답 파싱 실패(JSON 아님): {resp.text[:200]}") from e
        if not token:
            raise _NWError(f"access_token 없음: {resp.text[:300]}")
        return token

    def _get_events(self, token: str, ctx: CollectContext) -> list[CalendarEvent]:
        c = self.cfg
        # 선택된 캘린더들(복수). 없으면 기본 캘린더 하나.
        cal_ids = list(c.calendar_ids) or ([c.calendar_id] if c.calendar_id else [])

        events: list[CalendarEvent] = []
        if not cal_ids:
            events = self._fetch_calendar(token, ctx, None)
        else:
            for cid in cal_ids:
                events.extend(self._fetch_calendar(token, ctx, cid))

        events.sort(key=lambda e: (e.start or ""))
        return events

    def _fetch_calendar(self, token: str, ctx: CollectContext,
                        calendar_id: str | None) -> list[CalendarEvent]:
        c = self.cfg
        # RFC3339, tz 오프셋 포함. requests 가 params dict 인코딩 시 '+' 를 %2B 로 처리.
        from_dt = ctx.start.isoformat()
        until_dt = ctx.end.isoformat()  # 다음날 00:00 (범위 31일 이내 OK)

        if calendar_id:
            url = f"{API_BASE}/users/{quote(c.user_id)}/calendars/{quote(calendar_id)}/events"
        else:
            url = f"{API_BASE}/users/{quote(c.user_id)}/calendar/events"

        events: list[CalendarEvent] = []
        cursor = None
        for _ in range(50):  # 페이지네이션(cursor) — 이벤트가 페이지 상한을 넘어도 누락 없이 수집
            params = {"fromDateTime": from_dt, "untilDateTime": until_dt}
            if cursor:
                params["cursor"] = cursor
            try:
                resp = requests.get(
                    url,
                    headers={"Authorization": f"Bearer {token}"},
                    params=params,
                    timeout=30,
                )
            except requests.exceptions.RequestException as e:
                raise _NWError(str(e)) from e
            if resp.status_code != 200:
                raise _NWError(f"HTTP {resp.status_code}: {resp.text[:300]}")
            try:
                payload = resp.json()
            except ValueError as e:
                raise _NWError(f"캘린더 응답 파싱 실패(JSON 아님): {resp.text[:200]}") from e
            events.extend(_normalize_events(payload))
            cursor = (payload.get("responseMetaData") or {}).get("nextCursor") if isinstance(payload, dict) else None
            if not cursor:
                break
        return events


def _normalize_events(payload: dict) -> list[CalendarEvent]:
    out: list[CalendarEvent] = []
    for ev in (payload or {}).get("events", []) or []:
        for comp in ev.get("eventComponents", []) or []:
            start = comp.get("start", {}) or {}
            end = comp.get("end", {}) or {}
            all_day = "date" in start  # all-day 는 date, timed 는 dateTime
            attendees = [
                a.get("displayName") or a.get("email") or ""
                for a in (comp.get("attendees", []) or [])
            ]
            out.append(
                CalendarEvent(
                    title=comp.get("summary"),
                    start=start.get("date") if all_day else start.get("dateTime"),
                    end=end.get("date") if all_day else end.get("dateTime"),
                    all_day=all_day,
                    location=comp.get("location"),
                    description=comp.get("description"),
                    attendees=[a for a in attendees if a],
                )
            )
    # 시작 시간 순 정렬(문자열 비교로 충분: RFC3339)
    out.sort(key=lambda e: (e.start or ""))
    return out


class _NWError(Exception):
    pass
