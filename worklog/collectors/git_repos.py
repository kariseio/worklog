"""Git 수집기.

설정된 저장소들(repos) + 자동탐색(scan_roots) + Claude 로그에서 넘어온 cwd 를 대상으로
그날 만들어진 커밋을 `git log` 로 모은다.
"""

from __future__ import annotations

import os
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from ..config import GitConfig
from ..models import GitCommit, GitData
from ..util import CREATE_NO_WINDOW, git_common_dir, parse_iso, repo_root_of
from .base import CollectContext, Collector, CollectorResult

# git log 파서용 구분자
_REC = "\x1e"   # record separator (커밋 시작)
_FLD = "\x1f"   # field separator


def _workers(n: int) -> int:
    """동시 실행 스레드 수. git subprocess 는 I/O 바운드라 스레드로 잘 겹친다.

    과도한 동시 실행(디스크 seek 경합·프로세스 폭주) 방지를 위해 상한을 둔다.
    """
    if n <= 1:
        return 1
    return min(n, min(32, (os.cpu_count() or 4) * 4))


def _scan_many(roots: list[str], depth: int) -> list[str]:
    """스캔 루트 여러 개를 '각각 병렬로' 탐색해 .git 폴더 경로들을 합친다."""
    roots = [r for r in (roots or []) if r]
    if not roots:
        return []
    if len(roots) == 1:
        return _scan(roots[0], depth)
    out: list[str] = []
    with ThreadPoolExecutor(max_workers=_workers(len(roots))) as ex:
        for res in ex.map(lambda r: _scan(r, depth), roots):
            out.extend(res)
    return out


def _identify_many(paths: list[str]) -> list[tuple[str, str, str]]:
    """후보 경로들의 (경로, git-common-dir, 저장소명) 을 병렬로 구한다(저장소 아니면 제외).

    입력 순서를 보존하므로(중복 제거 시 first-seen) 결과가 결정론적이다.
    """
    if not paths:
        return []
    with ThreadPoolExecutor(max_workers=_workers(len(paths))) as ex:
        idents = list(ex.map(_repo_identity, paths))
    out: list[tuple[str, str, str]] = []
    for p, ident in zip(paths, idents):
        if ident:
            canonical, name = ident
            out.append((p, canonical, name))
    return out


class GitCollector(Collector):
    name = "git"

    def __init__(self, cfg: GitConfig, extra_repos: list[str] | None = None):
        self.cfg = cfg
        self.extra_repos = extra_repos or []

    def collect(self, ctx: CollectContext) -> CollectorResult:
        warnings: list[str] = []

        if not _git_available():
            return CollectorResult.skip(self.name, "git 실행 파일을 찾을 수 없습니다.")

        repos = self._resolve_repos(warnings)
        if not repos:
            return CollectorResult.skip(
                self.name,
                "감시할 git 저장소가 없습니다. config.yaml 의 sources.git.repos / scan_roots 를 설정하세요.",
            )

        # 저장소별 git log 를 병렬로. 개별 저장소 실패는 전체를 막지 않는다.
        def _one(item: tuple[str, str, str]):
            path, name, canonical = item
            try:
                return self._log(path, name, canonical, ctx), None
            except Exception as e:  # noqa: BLE001
                return [], f"git 로그 실패({path}): {e}"

        commits: list[GitCommit] = []
        with ThreadPoolExecutor(max_workers=_workers(len(repos))) as ex:
            for cs, warn in ex.map(_one, repos):
                commits.extend(cs)
                if warn:
                    warnings.append(warn)

        commits.sort(key=lambda c: c.when)
        return CollectorResult(name=self.name, data=GitData(commits=commits), warnings=warnings)

    # ------------------------------------------------------------------ #

    def _resolve_repos(self, warnings: list[str]) -> list[tuple[str, str, str]]:
        """(경로, 저장소명, 식별키) 목록. 같은 저장소의 여러 worktree 는 하나로 합친다.

        후보 경로를 (repos → scan_roots(각 루트 병렬) → Claude cwd) 순서로 모은 뒤,
        git 식별(git-common-dir)을 '저장소별 병렬'로 돌려 중복(worktree 포함)을 제거한다.
        """
        candidates: list[str] = []

        for r in self.cfg.repos:
            p = Path(r)
            if (p / ".git").exists():
                candidates.append(r)
            elif p.exists():
                warnings.append(f"git 저장소가 아님(.git 없음): {r}")
            else:
                warnings.append(f"경로 없음: {r}")

        # scan_all_drives 면 지정한 스캔 루트 대신 모든 고정 디스크를 훑는다.
        if getattr(self.cfg, "scan_all_drives", False):
            from ..util import fixed_drives
            roots = fixed_drives()
        else:
            roots = self.cfg.scan_roots
        candidates.extend(_scan_many(roots, self.cfg.scan_depth))

        if self.cfg.include_claude_cwds:
            for cwd in self.extra_repos:
                if cwd and (Path(cwd) / ".git").exists():
                    candidates.append(cwd)

        by_identity: dict[str, tuple[str, str, str]] = {}
        for path, canonical, name in _identify_many(candidates):
            by_identity.setdefault(canonical, (path, name, canonical))   # first-seen 유지
        return list(by_identity.values())

    def _log(self, repo: str, name: str, canonical: str, ctx: CollectContext) -> list[GitCommit]:
        # %cI(committer date): --since/--until 도 committer date 기준이라 기준을 일치시켜야
        # amend/rebase/cherry-pick 커밋이 엉뚱한 날에 잡히지 않는다.
        pretty = f"{_REC}%H{_FLD}%an{_FLD}%cI{_FLD}%s"
        # --branches: 로컬 브랜치 전부(worktree 브랜치 포함). HEAD 도 넣어 detached 체크아웃 포함.
        # remote-tracking/tag 는 제외해 노이즈 감소.
        cmd = [
            "git", "-C", repo, "log", "--branches", "HEAD", "--no-merges",
            f"--since={ctx.start.isoformat()}",
            f"--until={ctx.end.isoformat()}",
            "--numstat", f"--pretty=format:{pretty}",
        ]
        if self.cfg.author:
            cmd.insert(4, f"--author={self.cfg.author}")

        out = subprocess.run(
            cmd, capture_output=True, text=True, encoding="utf-8", errors="replace",
            creationflags=CREATE_NO_WINDOW,
        )
        if out.returncode != 0:
            raise RuntimeError((out.stderr or "").strip() or f"exit {out.returncode}")

        return _parse_log(out.stdout, repo_name=name, repo_path=canonical)


def _parse_log(stdout: str, repo_name: str, repo_path: str = "") -> list[GitCommit]:
    commits: list[GitCommit] = []
    # 각 레코드는 _REC 로 시작. 첫 조각은 빈 문자열.
    for chunk in stdout.split(_REC):
        chunk = chunk.strip("\n")
        if not chunk:
            continue
        lines = chunk.split("\n")
        header = lines[0].split(_FLD)
        if len(header) < 4:
            continue
        h, author, when_iso, subject = header[0], header[1], header[2], header[3]
        ins = dels = files = 0
        for stat in lines[1:]:
            stat = stat.strip()
            if not stat:
                continue
            parts = stat.split("\t")
            if len(parts) >= 3:
                files += 1
                a, d = parts[0], parts[1]
                if a.isdigit():
                    ins += int(a)
                if d.isdigit():
                    dels += int(d)
        when = parse_iso(when_iso)
        commits.append(
            GitCommit(
                repo=repo_name,
                hash=h,
                author=author,
                when=when,
                subject=subject,
                files_changed=files,
                insertions=ins,
                deletions=dels,
                repo_path=repo_path,
            )
        )
    return commits


def _repo_identity(path: str) -> tuple[str, str] | None:
    """저장소의 (정규 식별자=git-common-dir, 표시 이름=저장소 폴더명) 을 구한다.

    linked worktree 들은 같은 git-common-dir 을 공유하므로 이 값으로 중복 제거한다.
    (Claude 수집기·서비스 disambiguation 도 동일한 util.git_common_dir 키를 쓴다.)
    """
    common = git_common_dir(path)
    if not common:
        return None
    root = repo_root_of(common)
    name = Path(root).name or Path(path).resolve().name
    return common, name


def _scan(root: str, depth: int) -> list[str]:
    """root 아래 depth 깊이까지 .git 을 포함한 디렉토리를 찾는다."""
    results: list[str] = []
    root_path = Path(root)
    if not root_path.exists():
        return results

    def walk(path: Path, level: int):
        if (path / ".git").exists():
            results.append(str(path))
            return  # 저장소 안쪽은 더 안 내려감
        if level >= depth:
            return
        try:
            for child in path.iterdir():
                if child.is_dir() and not child.name.startswith("."):
                    walk(child, level + 1)
        except (PermissionError, OSError):
            pass

    walk(root_path, 0)
    return results


def _git_available() -> bool:
    import shutil

    return shutil.which("git") is not None
