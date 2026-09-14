//! 세션 질답(Q/A) 누적기 — Claude Code · Codex 수집기가 공유한다.
//!
//! 사용자 프롬프트를 '질문'으로, 다음 프롬프트 전까지의 어시스턴트 프로즈를 '답 요지'로 묶는다.
//! 전날 밤 프롬프트의 답이 자정을 넘겨 오늘 시작되는 경우를 위해, 날짜와 무관하게 마지막
//! 사용자 프롬프트를 `carry` 로 기억해 두었다가 이어붙인다.

use crate::model::QaTurn;

/// 문자열을 유니코드 문자 기준 `n` 자로 자른다(바이트가 아니라 글자 수 — Python 슬라이스와 동일).
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 공백(개행 포함)을 하나로 정리한다 (`" ".join(s.split())`).
pub fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone)]
struct Current {
    time: String,
    question: String,
    answers: Vec<String>,
    answer_chars: usize,
}

#[derive(Debug, Clone)]
pub struct QaAccumulator {
    turns: Vec<QaTurn>,
    cur: Option<Current>,
    /// (시각, 질문) — 날짜 무관 마지막 사용자 프롬프트.
    carry: Option<(String, String)>,
    max_intent_len: usize,
    max_answer_len: usize,
}

impl QaAccumulator {
    pub fn new(max_intent_len: usize, max_answer_len: usize) -> Self {
        Self {
            turns: Vec::new(),
            cur: None,
            carry: None,
            max_intent_len,
            max_answer_len,
        }
    }

    /// 날짜 무관: 마지막 '진짜' 사용자 프롬프트를 기억한다(자정 연속 대비).
    pub fn remember_carry(&mut self, time: &str, question: &str) {
        self.carry = Some((
            time.to_string(),
            truncate_chars(question, self.max_intent_len),
        ));
    }

    /// 대상일의 사용자 프롬프트 → 직전 질답을 닫고 새 질답 시작. 잘린 질문 텍스트를 돌려준다.
    pub fn start_question(&mut self, time: &str, question: &str) -> String {
        self.flush();
        let q = truncate_chars(question, self.max_intent_len);
        self.cur = Some(Current {
            time: time.to_string(),
            question: q.clone(),
            answers: Vec::new(),
            answer_chars: 0,
        });
        q
    }

    /// 진행 중인 질답이 없고 carry 가 있으면 carry 로 질답을 연다.
    /// 열었으면 그 질문 텍스트를 돌려준다(intent 후보).
    pub fn open_from_carry_if_needed(&mut self) -> Option<String> {
        if self.cur.is_some() {
            return None;
        }
        let (time, q) = self.carry.clone()?;
        self.cur = Some(Current {
            time,
            question: q.clone(),
            answers: Vec::new(),
            answer_chars: 0,
        });
        Some(q)
    }

    /// 어시스턴트 프로즈 추가. 답 요지 상한의 3배까지만 모은다(메모리).
    pub fn push_answer(&mut self, text: &str) {
        if let Some(cur) = &mut self.cur
            && cur.answer_chars < self.max_answer_len * 3
        {
            cur.answer_chars += text.chars().count();
            cur.answers.push(text.to_string());
        }
    }

    pub fn has_current(&self) -> bool {
        self.cur.is_some()
    }

    fn flush(&mut self) {
        if let Some(cur) = self.cur.take() {
            let joined = cur.answers.join(" ");
            let answer = truncate_chars(&squash_ws(&joined), self.max_answer_len);
            self.turns.push(QaTurn {
                time: cur.time,
                question: cur.question,
                answer,
            });
        }
    }

    /// 마무리: 상한을 넘으면 '최근' 질답을 남기고 앞부분을 버린다. (하루 끝 결과 보존)
    /// 반환: (질답 목록, 생략된 수).
    pub fn finish(mut self, max_qa_turns: usize) -> (Vec<QaTurn>, u32) {
        self.flush();
        let total = self.turns.len();
        if total > max_qa_turns {
            let dropped = total - max_qa_turns;
            let kept = self.turns.split_off(dropped);
            return (kept, dropped as u32);
        }
        (self.turns, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_questions_with_following_answers() {
        let mut acc = QaAccumulator::new(300, 180);
        acc.start_question("10:00", "첫 요청 내용");
        acc.push_answer("첫 응답\n요지");
        acc.push_answer("계속");
        acc.start_question("11:00", "둘째 다른 주제");
        acc.push_answer("둘째 응답");
        let (turns, dropped) = acc.finish(120);
        assert_eq!(dropped, 0);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].question, "첫 요청 내용");
        assert_eq!(turns[0].answer, "첫 응답 요지 계속");
        assert_eq!(turns[0].time, "10:00");
        assert_eq!(turns[1].answer, "둘째 응답");
    }

    #[test]
    fn cap_keeps_recent_not_head() {
        let mut acc = QaAccumulator::new(300, 180);
        for i in 0..5 {
            acc.start_question(&format!("0{i}:00"), &format!("주제{i}"));
            acc.push_answer(&format!("답{i}"));
        }
        let (turns, dropped) = acc.finish(3);
        assert_eq!(dropped, 2);
        assert_eq!(
            turns
                .iter()
                .map(|t| t.question.as_str())
                .collect::<Vec<_>>(),
            vec!["주제2", "주제3", "주제4"]
        );
    }

    #[test]
    fn carry_opens_answer_that_crosses_midnight() {
        let mut acc = QaAccumulator::new(300, 180);
        acc.remember_carry("23:55", "자정 넘기는 질문");
        assert!(!acc.has_current());
        assert_eq!(
            acc.open_from_carry_if_needed().as_deref(),
            Some("자정 넘기는 질문")
        );
        assert!(acc.has_current());
        assert_eq!(acc.open_from_carry_if_needed(), None); // 이미 열려 있으면 안 연다
        acc.push_answer("자정 후 답");
        let (turns, _) = acc.finish(10);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].time, "23:55");
        assert_eq!(turns[0].answer, "자정 후 답");

        // carry 가 없으면 열지 않는다
        let mut acc2 = QaAccumulator::new(300, 180);
        assert_eq!(acc2.open_from_carry_if_needed(), None);
        acc2.push_answer("버려짐"); // 진행 중 질답 없음 → 무시
        assert_eq!(acc2.finish(10).0.len(), 0);
    }

    #[test]
    fn truncation_is_by_chars_and_answers_are_bounded() {
        let mut acc = QaAccumulator::new(5, 4);
        let q = acc.start_question("", "가나다라마바사");
        assert_eq!(q, "가나다라마");
        for _ in 0..100 {
            acc.push_answer("답변답변");
        }
        let (turns, _) = acc.finish(10);
        assert_eq!(turns[0].answer, "답변답변"); // max_answer_len 4
        assert!(turns[0].time.is_empty());
        assert_eq!(squash_ws("  a \n b\t c "), "a b c");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}
