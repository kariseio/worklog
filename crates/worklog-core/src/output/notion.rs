//! Notion 출력: 새 페이지를 만들어 업무일지를 기록한다.
//!
//! parent_type=page     → 부모 페이지의 하위 페이지로 생성
//! parent_type=database → 데이터베이스의 한 행(row)으로 생성 (title_prop 에 제목)
//!
//! 주의: 통합(Integration)을 대상 페이지/DB 에 '연결(Connections)' 해두지 않으면 404.
//!       children 는 요청당 최대 100블록이라 배치로 나눠 append 한다.

use std::{sync::LazyLock, time::Duration};

use regex::Regex;
use serde_json::{Value, json};

use super::{Sink, SinkResult};
use crate::{config::NotionOutputConfig, model::WorkLog};

pub const API: &str = "https://api.notion.com/v1";
const MAX_BLOCKS: usize = 100;
/// 한 text 오브젝트는 2000자 제한 → 여유 두고 자름.
const MAX_RICH_TEXT: usize = 1900;

static DIVIDER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-{3,}$").expect("regex"));
static TABLE_SEP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:?-{2,}:?$").expect("regex"));
static BOLD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").expect("regex"));
static CODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`(.+?)`").expect("regex"));
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(.+?)\]\((.+?)\)").expect("regex"));

fn snippet(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// --------------------------------------------------------------------------- //
// Markdown → Notion 블록 (간단 변환기)
// --------------------------------------------------------------------------- //

pub fn markdown_to_blocks(md: &str) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut in_code = false;
    let mut code_lines: Vec<&str> = Vec::new();
    let mut code_lang = "plain text".to_string();

    for raw in md.lines() {
        let line = raw.trim_end_matches('\n');
        let fence = line.trim();
        if let Some(rest) = fence.strip_prefix("```") {
            if !in_code {
                in_code = true;
                code_lines.clear();
                code_lang = notion_lang(&rest.trim().to_lowercase());
            } else {
                blocks.push(code_block(&code_lines.join("\n"), &code_lang));
                in_code = false;
            }
            continue;
        }
        if in_code {
            code_lines.push(line);
            continue;
        }

        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        if let Some(t) = stripped.strip_prefix("### ") {
            blocks.push(heading(t, 3));
        } else if let Some(t) = stripped.strip_prefix("## ") {
            blocks.push(heading(t, 2));
        } else if let Some(t) = stripped.strip_prefix("# ") {
            blocks.push(heading(t, 1));
        } else if DIVIDER_RE.is_match(stripped) {
            // 하이픈으로만 된 줄만 divider
            blocks.push(json!({"object": "block", "type": "divider", "divider": {}}));
        } else if stripped.starts_with("- ") || stripped.starts_with("* ") {
            // 들여쓰기 bullet 도 평면 bullet 으로 (2단계 이상 중첩은 단순화)
            blocks.push(bullet(&stripped[2..]));
        } else if stripped == "<details>"
            || stripped == "</details>"
            || stripped.starts_with("<summary")
        {
            continue; // 데이터 접기 HTML 래퍼는 Notion 에서 리터럴 텍스트로 새므로 버린다
        } else if stripped.starts_with('|') && stripped.ends_with('|') && stripped.len() > 1 {
            // GFM 표: 구분선(|---|)은 버리고, 데이터 행은 셀을 ' · ' 로 이어 가독 문단으로.
            let cells: Vec<&str> = stripped
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect();
            if !cells.is_empty() && cells.iter().all(|c| TABLE_SEP_RE.is_match(c)) {
                continue;
            }
            blocks.push(paragraph(&cells.join(" · ")));
        } else {
            blocks.push(paragraph(stripped));
        }
    }

    if in_code {
        // 닫히지 않은 코드펜스 방어
        blocks.push(code_block(&code_lines.join("\n"), &code_lang));
    }
    blocks
}

/// plain text → rich_text 배열. 2000자 제한을 넘으면 여러 조각으로 분할하고 인라인 마크다운은 제거.
pub fn rich_text(text: &str) -> Vec<Value> {
    rich_text_raw(&strip_inline(text))
}

/// 인라인 마크다운 제거 없이 2000자 분할만. (코드블록처럼 원문 그대로 넣어야 할 때)
pub fn rich_text_raw(text: &str) -> Vec<Value> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    if chars.is_empty() {
        return vec![json!({"type": "text", "text": {"content": ""}})];
    }
    for chunk in chars.chunks(MAX_RICH_TEXT) {
        out.push(json!({"type": "text", "text": {"content": chunk.iter().collect::<String>()}}));
    }
    out
}

/// `**bold**`, `` `code` ``, `[label](url)` 같은 인라인은 표기만 정리해 가독성 유지.
fn strip_inline(text: &str) -> String {
    let t = BOLD_RE.replace_all(text, "$1");
    let t = CODE_RE.replace_all(&t, "$1");
    LINK_RE.replace_all(&t, "$1").into_owned()
}

fn heading(text: &str, level: u8) -> Value {
    let key = format!("heading_{}", level.min(3));
    json!({"object": "block", "type": key, key: {"rich_text": rich_text(text)}})
}

fn bullet(text: &str) -> Value {
    json!({"object": "block", "type": "bulleted_list_item",
           "bulleted_list_item": {"rich_text": rich_text(text)}})
}

fn paragraph(text: &str) -> Value {
    json!({"object": "block", "type": "paragraph", "paragraph": {"rich_text": rich_text(text)}})
}

fn code_block(text: &str, lang: &str) -> Value {
    json!({"object": "block", "type": "code",
           "code": {"rich_text": rich_text_raw(text), "language": lang}}) // 코드 원문 보존
}

fn notion_lang(lang: &str) -> String {
    match lang {
        "py" | "python" => "python",
        "js" | "javascript" => "javascript",
        "ts" | "typescript" => "typescript",
        "bash" => "bash",
        "sh" | "shell" => "shell",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "sql" => "sql",
        _ => "plain text",
    }
    .to_string()
}

// --------------------------------------------------------------------------- //
// API
// --------------------------------------------------------------------------- //

fn extract_title(obj: &Value) -> Option<String> {
    // database: obj["title"] 는 rich_text 배열. page: properties 안의 title 타입.
    let join = |arr: &Vec<Value>| -> Option<String> {
        let s: String = arr
            .iter()
            .filter_map(|t| t.get("plain_text").and_then(Value::as_str))
            .collect();
        (!s.is_empty()).then_some(s)
    };
    if let Some(Value::Array(arr)) = obj.get("title")
        && !arr.is_empty()
    {
        return join(arr);
    }
    let props = obj.get("properties").and_then(Value::as_object)?;
    for prop in props.values() {
        if prop.get("type").and_then(Value::as_str) == Some("title") {
            return prop.get("title").and_then(Value::as_array).and_then(join);
        }
    }
    None
}

pub struct NotionSink {
    cfg: NotionOutputConfig,
    api: String,
}

impl NotionSink {
    pub fn new(cfg: NotionOutputConfig) -> Self {
        Self {
            cfg,
            api: API.into(),
        }
    }

    /// 테스트용: 엔드포인트 교체.
    pub fn with_api(mut self, api: impl Into<String>) -> Self {
        self.api = api.into();
        self
    }

    fn client(&self) -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    }

    fn request(&self, method: reqwest::Method, url: &str, body: &Value) -> Result<Value, String> {
        let resp = self
            .client()
            .request(method, url)
            .bearer_auth(&self.cfg.token)
            .header("Notion-Version", &self.cfg.version)
            .json(body)
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.as_u16() >= 300 {
            return Err(format!("HTTP {}: {}", status.as_u16(), snippet(&text, 400)));
        }
        serde_json::from_str(&text).map_err(|e| format!("응답 파싱 실패: {e}"))
    }

    fn create_page(&self, title: &str, blocks: &[Value]) -> Result<Value, String> {
        let body = if self.cfg.parent_type == "database" {
            json!({
                "parent": {"database_id": self.cfg.parent_id},
                "properties": {self.cfg.title_prop.clone(): {"title": rich_text(title)}},
                "children": blocks,
            })
        } else {
            json!({
                "parent": {"page_id": self.cfg.parent_id},
                "properties": {"title": {"title": rich_text(title)}},
                "children": blocks,
            })
        };
        self.request(reqwest::Method::POST, &format!("{}/pages", self.api), &body)
    }

    fn append(&self, block_id: &str, blocks: &[Value]) -> Result<Value, String> {
        self.request(
            reqwest::Method::PATCH,
            &format!("{}/blocks/{block_id}/children", self.api),
            &json!({"children": blocks}),
        )
    }

    /// 실제 Notion API 로 토큰 + 대상(page/database) 접근을 확인.
    pub fn test_connection(&self) -> (bool, String) {
        if self.cfg.token.trim().is_empty() {
            return (false, "Notion 토큰을 입력하세요.".into());
        }
        if self.cfg.parent_id.trim().is_empty() {
            return (false, "대상 page_id / database_id 를 입력하세요.".into());
        }
        let kind = if self.cfg.parent_type == "database" {
            "databases"
        } else {
            "pages"
        };
        let url = format!("{}/{kind}/{}", self.api, self.cfg.parent_id);
        let resp = self
            .client()
            .get(&url)
            .bearer_auth(&self.cfg.token)
            .header("Notion-Version", &self.cfg.version)
            .send();
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return (false, format!("네트워크 오류: {e}")),
        };
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        match status {
            200 => match serde_json::from_str::<Value>(&text) {
                Ok(data) => {
                    let title = extract_title(&data).unwrap_or_else(|| "(제목 없음)".into());
                    let what = if kind == "databases" { "DB" } else { "페이지" };
                    (true, format!("연결됨 · {what} '{title}'"))
                }
                Err(_) => (false, format!("예상치 못한 응답 (HTTP 200, JSON 아님): {}", snippet(&text, 200))),
            },
            401 => (false, "토큰이 유효하지 않습니다 (401).".into()),
            404 => (
                false,
                "대상을 찾을 수 없습니다. 통합을 페이지/DB 에 '연결(Connections)' 했는지 확인하세요 (404).".into(),
            ),
            other => (false, format!("실패 (HTTP {other}): {}", snippet(&text, 200))),
        }
    }
}

impl Sink for NotionSink {
    fn name(&self) -> &'static str {
        "notion"
    }

    fn write(&self, worklog: &WorkLog) -> SinkResult {
        if self.cfg.token.trim().is_empty() {
            return SinkResult::failure(self.name(), "Notion 토큰 미설정");
        }
        if self.cfg.parent_id.trim().is_empty() {
            return SinkResult::failure(self.name(), "outputs.notion.parent_id 미설정");
        }
        let title = format!("업무일지 {}", worklog.target_date);
        let blocks = markdown_to_blocks(&worklog.full_markdown);
        let first: Vec<Value> = blocks.iter().take(MAX_BLOCKS).cloned().collect();
        let page = match self.create_page(&title, &first) {
            Ok(p) => p,
            Err(e) => return SinkResult::failure(self.name(), e),
        };
        let page_id = page
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 100블록 초과분은 append
        for chunk in blocks
            .iter()
            .skip(MAX_BLOCKS)
            .collect::<Vec<_>>()
            .chunks(MAX_BLOCKS)
        {
            let batch: Vec<Value> = chunk.iter().map(|v| (*v).clone()).collect();
            if let Err(e) = self.append(&page_id, &batch) {
                return SinkResult::failure(self.name(), format!("블록 추가 실패: {e}"));
            }
        }
        let location = page
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| {
                if page_id.is_empty() {
                    "(created)".into()
                } else {
                    page_id.clone()
                }
            });
        SinkResult::success(self.name(), location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(blocks: &[Value]) -> Vec<String> {
        blocks
            .iter()
            .map(|b| b["type"].as_str().unwrap().to_string())
            .collect()
    }

    fn text_of(b: &Value) -> String {
        let t = b["type"].as_str().unwrap();
        b[t]["rich_text"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| x["text"]["content"].as_str().unwrap_or(""))
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    #[test]
    fn markdown_to_blocks_basic() {
        let md =
            "# 제목\n\n## 소제목\n\n- 항목1\n- 항목2\n\n일반 문단\n\n```python\nprint(1)\n```\n";
        let blocks = markdown_to_blocks(md);
        let t = types(&blocks);
        assert!(t.contains(&"heading_1".to_string()));
        assert!(t.contains(&"heading_2".to_string()));
        assert_eq!(t.iter().filter(|x| *x == "bulleted_list_item").count(), 2);
        assert!(t.contains(&"paragraph".to_string()));
        let code = blocks.iter().find(|b| b["type"] == "code").unwrap();
        assert_eq!(code["code"]["language"], "python");
        assert_eq!(code["code"]["rich_text"][0]["text"]["content"], "print(1)");
        assert_eq!(text_of(&blocks[0]), "제목");
    }

    #[test]
    fn code_block_preserves_inline_markdown_and_unclosed_fence() {
        let blocks = markdown_to_blocks("```python\ny = '**b**' + `c` + '[x](u)'\n```\n");
        let code = blocks.iter().find(|b| b["type"] == "code").unwrap();
        let content = text_of(code);
        assert!(content.contains("**b**") && content.contains("`c`") && content.contains("[x](u)"));
        let blocks = markdown_to_blocks("```\nopen\n");
        assert_eq!(types(&blocks), vec!["code"]);
        assert_eq!(blocks[0]["code"]["language"], "plain text");
        // 인라인은 문단에서 정리된다
        let p = markdown_to_blocks("**굵게** 와 `코드` 와 [링크](http://x)\n");
        assert_eq!(text_of(&p[0]), "굵게 와 코드 와 링크");
    }

    #[test]
    fn divider_rules() {
        assert!(
            !types(&markdown_to_blocks("--- 중요: 이건 divider 가 아님\n"))
                .contains(&"divider".to_string())
        );
        assert!(types(&markdown_to_blocks("--- 중요\n")).contains(&"paragraph".to_string()));
        assert!(types(&markdown_to_blocks("---\n")).contains(&"divider".to_string()));
        assert!(types(&markdown_to_blocks("-----\n")).contains(&"divider".to_string()));
    }

    #[test]
    fn skips_details_and_converts_tables() {
        let md = [
            "<details>",
            "<summary>원본</summary>",
            "| 프로젝트 | 커밋 |",
            "|---|---|",
            "| A | 3 |",
            "</details>",
        ]
        .join("\n");
        let blocks = markdown_to_blocks(&md);
        let joined = blocks.iter().map(text_of).collect::<Vec<_>>().join(" ");
        assert!(!joined.contains("<details>") && !joined.contains("<summary>"));
        assert!(!joined.contains("---"));
        assert!(joined.contains("프로젝트 · 커밋"));
        assert!(joined.contains("A · 3"));
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn rich_text_splits_long_text() {
        let long = "가".repeat(5000);
        let rts = rich_text(&long);
        assert!(rts.len() >= 3);
        assert!(
            rts.iter()
                .all(|r| r["text"]["content"].as_str().unwrap().chars().count() <= 2000)
        );
        assert_eq!(rich_text_raw("").len(), 1);
        assert_eq!(heading("x", 9)["type"], "heading_3");
    }

    #[test]
    fn title_extraction_and_precondition_messages() {
        assert_eq!(
            extract_title(&json!({"title": [{"plain_text": "DB "}, {"plain_text": "이름"}]}))
                .as_deref(),
            Some("DB 이름")
        );
        assert_eq!(
            extract_title(&json!({"properties": {"Name": {"type": "title", "title": [{"plain_text": "페이지"}]}}})).as_deref(),
            Some("페이지")
        );
        assert_eq!(extract_title(&json!({"title": []})), None);
        let s = NotionSink::new(NotionOutputConfig::default());
        assert!(!s.test_connection().0);
        let wl = WorkLog {
            target_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(),
            facts_markdown: String::new(),
            full_markdown: "# x".into(),
            data: crate::model::DailyData::new(
                chrono::NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(),
                "Asia/Seoul",
            ),
            summary_markdown: None,
        };
        assert!(s.write(&wl).error.unwrap().contains("토큰"));
        let s2 = NotionSink::new(NotionOutputConfig {
            token: "t".into(),
            ..Default::default()
        });
        assert!(s2.write(&wl).error.unwrap().contains("parent_id"));
    }
}
