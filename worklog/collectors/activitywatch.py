"""ActivityWatch 수집기.

로컬 aw-server(REST, 기본 http://localhost:5600)에서 그날의 앱별 실사용 시간을 가져온다.
window watcher(활성 창)와 afk watcher(자리비움)를 query2 로 교차시켜
'실제로 자리에 있던 시간'만 집계한다. 서버가 없으면 조용히 건너뛴다.

참고: docs.activitywatch.net (REST / query2)
"""

from __future__ import annotations

from ..config import ActivityWatchConfig
from ..models import ActivityWatchData, AppUsage
from .base import CollectContext, Collector, CollectorResult

try:
    import requests
except ImportError:  # pragma: no cover
    requests = None  # type: ignore


class ActivityWatchCollector(Collector):
    name = "activitywatch"

    def __init__(self, cfg: ActivityWatchConfig):
        self.cfg = cfg

    def collect(self, ctx: CollectContext) -> CollectorResult:
        if requests is None:
            return CollectorResult.fail(self.name, "requests 미설치")

        base = self.cfg.base_url.rstrip("/") + "/api/0"

        # 1) 버킷 목록 조회 → type 으로 창/afk 버킷 식별
        try:
            resp = requests.get(f"{base}/buckets/", timeout=5)
            resp.raise_for_status()
            buckets = resp.json()
        except requests.exceptions.RequestException:
            return CollectorResult.skip(
                self.name,
                f"ActivityWatch 서버에 연결할 수 없습니다 ({self.cfg.base_url}). 미실행/미설치일 수 있습니다.",
            )

        window_bucket = _pick(buckets, "currentwindow")
        afk_bucket = _pick(buckets, "afkstatus")

        if not window_bucket:
            return CollectorResult.skip(
                self.name, "aw-watcher-window 버킷을 찾지 못했습니다."
            )

        hostname = None
        meta = buckets.get(window_bucket)
        if isinstance(meta, dict):
            hostname = meta.get("hostname")

        # 2) query2: 창 이벤트 ∩ not-afk, 앱+제목으로 병합
        timeperiod = f"{ctx.start.isoformat()}/{ctx.end.isoformat()}"
        lines = [f'window = flood(query_bucket("{window_bucket}"));']
        if afk_bucket:
            lines += [
                f'afk = flood(query_bucket("{afk_bucket}"));',
                'afk = filter_keyvals(afk, "status", ["not-afk"]);',
                "active = filter_period_intersect(window, afk);",
            ]
        else:
            lines += ["active = window;"]
        lines += [
            'active = merge_events_by_keys(active, ["app", "title"]);',
            "RETURN = sort_by_duration(active);",
        ]
        query = "\n".join(lines)

        try:
            resp = requests.post(
                f"{base}/query/",
                json={"timeperiods": [timeperiod], "query": query.split("\n")},
                timeout=30,
            )
            resp.raise_for_status()
            events = resp.json()[0]
        except (requests.exceptions.RequestException, IndexError, ValueError) as e:
            return CollectorResult.fail(self.name, f"ActivityWatch query 실패: {e}")

        data = self._aggregate(events, hostname)
        return CollectorResult(name=self.name, data=data)

    def _aggregate(self, events: list, hostname: str | None) -> ActivityWatchData:
        # (app,title) 이벤트를 app 단위로 합치고, 대표 제목 몇 개를 추린다.
        per_app: dict[str, dict] = {}
        total = 0.0
        for ev in events:
            if not isinstance(ev, dict):
                continue
            dur = float(ev.get("duration", 0.0) or 0.0)
            total += dur
            data = ev.get("data", {}) or {}
            app = data.get("app") or "(알 수 없음)"
            title = data.get("title") or ""
            slot = per_app.setdefault(app, {"seconds": 0.0, "titles": {}})
            slot["seconds"] += dur
            if title:
                slot["titles"][title] = slot["titles"].get(title, 0.0) + dur

        apps: list[AppUsage] = []
        for app, slot in per_app.items():
            if slot["seconds"] < self.cfg.min_seconds:
                continue
            top_titles = [
                t for t, _ in sorted(slot["titles"].items(), key=lambda kv: kv[1], reverse=True)
            ][:3]
            apps.append(AppUsage(app=app, seconds=slot["seconds"], top_titles=top_titles))

        apps.sort(key=lambda a: a.seconds, reverse=True)
        apps = apps[: self.cfg.top_n]
        return ActivityWatchData(
            total_active_seconds=total, by_app=apps, hostname=hostname
        )


def _pick(buckets: dict, bucket_type: str) -> str | None:
    """buckets 딕셔너리에서 주어진 type 의 버킷 ID 를 찾는다."""
    for bid, meta in buckets.items():
        if isinstance(meta, dict) and meta.get("type") == bucket_type:
            return bid
    return None
