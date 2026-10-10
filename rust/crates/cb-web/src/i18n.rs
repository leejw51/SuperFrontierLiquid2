//! Two languages, one dictionary.
//!
//! English is the source: every string in the views is written in English,
//! and [`t`] looks it up in the Korean table when Korean is chosen. A string
//! with no entry falls back to English, so an untranslated corner is legible
//! rather than blank, and adding a language is adding a table.
//!
//! The choice is remembered in `localStorage` and applied as `lang` on
//! `<html>`, which is what lets the stylesheet swap in a Hangul-capable pixel
//! face. Changing it reloads the page: the views are written with plain
//! string literals wrapped in [`t`], and a reload is simpler and more honest
//! than threading a signal through three hundred call sites.

use gloo_storage::{LocalStorage, Storage};

/// The `localStorage` key the language is kept under.
pub const LANG_KEY: &str = "cbl.lang";

/// A language the client speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    /// The default, and the source language of every string.
    #[default]
    En,
    /// Korean.
    Ko,
}

impl Lang {
    /// Every language, in switch order.
    pub const ALL: [Self; 2] = [Self::En, Self::Ko];

    /// The BCP 47 tag, which is also the stored value.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ko => "ko",
        }
    }

    /// The name of the language, in that language.
    #[must_use]
    pub const fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ko => "한국어",
        }
    }

    /// Parse a stored or query value.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en-gb" => Some(Self::En),
            "ko" | "ko-kr" | "kr" => Some(Self::Ko),
            _ => None,
        }
    }
}

thread_local! {
    static CURRENT: std::cell::Cell<Lang> = const { std::cell::Cell::new(Lang::En) };
}

/// The language in use.
#[must_use]
pub fn current() -> Lang {
    CURRENT.with(std::cell::Cell::get)
}

/// The stored language, or English.
#[must_use]
pub fn stored() -> Lang {
    LocalStorage::get::<String>(LANG_KEY)
        .ok()
        .and_then(|value| Lang::parse(&value))
        .unwrap_or_default()
}

/// Read the stored choice (or `?lang=` in the URL, which wins and is
/// remembered) and apply it to `<html lang>`. Called once before mounting.
pub fn apply_stored() {
    let lang = crate::config::query_param("lang")
        .and_then(|value| Lang::parse(&value))
        .unwrap_or_else(stored);
    let _ = LocalStorage::set(LANG_KEY, lang.tag());
    CURRENT.with(|cell| cell.set(lang));
    if let Some(root) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.document_element())
    {
        let _ = root.set_attribute("lang", lang.tag());
    }
}

/// Choose a language, remember it, and reload so every view re-renders in it.
pub fn choose(lang: Lang) {
    let _ = LocalStorage::set(LANG_KEY, lang.tag());
    CURRENT.with(|cell| cell.set(lang));
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

/// Translate an English string into the current language.
///
/// Returns the input unchanged in English and for anything the table does not
/// know, so a missing translation is never a missing word.
#[must_use]
pub fn t(english: &'static str) -> &'static str {
    match current() {
        Lang::En => english,
        Lang::Ko => ko(english).unwrap_or(english),
    }
}

/// Translate an owned string, for text built at runtime.
#[must_use]
pub fn ts(english: &str) -> String {
    match current() {
        Lang::En => english.to_string(),
        Lang::Ko => KO
            .iter()
            .find(|(en, _)| *en == english)
            .map_or_else(|| english.to_string(), |(_, ko)| (*ko).to_string()),
    }
}

fn ko(english: &str) -> Option<&'static str> {
    KO.iter().find(|(en, _)| *en == english).map(|(_, ko)| *ko)
}

/// The Korean table. Keys are the exact English strings the views use.
const KO: &[(&str, &str)] = &[
    // ---- chrome
    ("Trade", "거래"),
    ("Portfolio", "포트폴리오"),
    ("Faucet", "수도꼭지"),
    ("Leaderboard", "순위표"),
    ("Dashboard", "대시보드"),
    ("Account", "계정"),
    ("Admin", "관리자"),
    ("Menu", "메뉴"),
    ("Sign in", "로그인"),
    ("Sign out", "로그아웃"),
    ("Equity", "자산"),
    ("Compact", "촘촘하게"),
    ("Light", "밝은 테마"),
    ("Language", "언어"),
    ("Live", "실시간"),
    ("Height", "블록 높이"),
    ("Throughput", "처리량"),
    ("Block time", "블록 시간"),
    ("Mempool", "멤풀"),
    ("Validators", "검증자"),
    ("State root", "상태 루트"),
    ("Mock data", "모의 데이터"),
    ("Live gateway", "실시간 게이트웨이"),
    // ---- landing
    ("How it works", "작동 방식"),
    ("The order book is real.", "주문장은 진짜입니다."),
    ("The money is not.", "돈은 진짜가 아닙니다."),
    ("Insert coin \u{25B8} press start", "코인을 넣고 \u{25B8} 스타트를 누르세요"),
    (
        "SuperFrontierLiquid is a personal experiment in exchange and blockchain design. Run this prototype locally, claim simulated funds, and explore spot, perpetual and dated-futures trading against bots. Use disposable test wallets only; this unfinished software is not suitable for real funds.",
        "SuperFrontierLiquid은 거래소와 블록체인 설계를 탐구하는 개인 실험입니다. 이 프로토타입을 로컬에서 실행하고, 모의 자금을 받아 봇을 상대로 현물, 무기한 선물, 만기 선물 거래를 살펴보세요. 일회용 테스트 지갑만 사용하세요. 이 미완성 소프트웨어는 실제 자금을 다루기에 적합하지 않습니다.",
    ),
    ("blocks, and every order is one transaction in one of them", "블록마다 모든 주문이 하나의 트랜잭션으로 담깁니다"),
    ("markets across spot, perpetual and dated futures", "개의 현물·무기한·만기 선물 시장"),
    ("personal data. No email and no password — a signature from your own key is the login", "개의 개인정보. 이메일도 비밀번호도 없이, 내 키의 서명이 곧 로그인입니다"),
    ("I need an account", "계정이 필요해요"),
    ("I have a recovery phrase", "복구 문구가 있어요"),
    ("Create a wallet", "지갑 만들기"),
    ("Paste a phrase", "문구 붙여넣기"),
    (
        "This generates twelve words and derives an address from them. Write the words down: they are the account, and they are shown once.",
        "열두 단어를 만들고 그로부터 주소를 유도합니다. 단어를 꼭 적어 두세요. 그 단어가 곧 계정이며, 한 번만 보여 드립니다.",
    ),
    (
        "Twelve BIP-39 words, from here or from any other wallet. Paste them and sign in. The phrase is checked in the browser and never sent.",
        "여기서 만든 것이든 다른 지갑의 것이든 BIP-39 열두 단어를 붙여넣고 로그인하세요. 문구는 브라우저 안에서만 확인되며 전송되지 않습니다.",
    ),
    ("Start with a hundred thousand of nothing", "가짜 돈 십만 달러로 시작하세요"),
    ("Take leverage you would never take", "평소라면 절대 안 쓸 레버리지를 써 보세요"),
    ("Look underneath any time you like", "언제든 밑바닥까지 들여다보세요"),
    (
        "The faucet grants play funds on an hourly cooldown, recorded in an append-only ledger. Run out and you wait an hour, the same as everyone else.",
        "수도꼭지는 한 시간 쿨다운으로 연습용 자금을 지급하고, 추가만 가능한 장부에 기록합니다. 다 쓰면 다른 사람처럼 한 시간을 기다립니다.",
    ),
    (
        "Up to fifty times, cross or isolated margin, with funding accruing every hour and a liquidation engine that does not care how you feel about it.",
        "최대 50배, 교차 또는 격리 마진, 매시간 쌓이는 펀딩, 그리고 당신 기분은 상관하지 않는 청산 엔진.",
    ),
    (
        "Orders, fills, funding and liquidations are transactions in blocks. The chain rail sits along the bottom of every screen so you can watch yours land.",
        "주문, 체결, 펀딩, 청산은 모두 블록 속 트랜잭션입니다. 화면 아래 체인 레일에서 내 트랜잭션이 담기는 순간을 볼 수 있습니다.",
    ),
    ("Claim", "받기"),
    ("Markets", "시장"),
    ("no trades yet", "아직 거래 없음"),
    ("This is the order book from the terminal itself, at the density you will trade in. Nothing on this page is a picture of the product.", "이것은 터미널의 실제 주문장이며, 거래할 때와 같은 밀도입니다. 이 페이지의 어떤 것도 그림이 아닙니다."),
    // ---- sign-in flow
    ("Sign in with your recovery phrase", "복구 문구로 로그인"),
    ("Recovery phrase", "복구 문구"),
    ("Cancel", "취소"),
    ("Continue", "계속"),
    ("Back", "뒤로"),
    ("Sign and continue", "서명하고 계속"),
    ("Sign again and continue", "다시 서명하고 계속"),
    ("Pick a handle", "닉네임 선택"),
    ("of 12 words", "/ 12 단어"),
    ("Checksum valid", "체크섬 유효"),
    ("Keep going", "계속 입력"),
    ("Checksum does not match", "체크섬 불일치"),
    ("Twelve BIP-39 words, from here or from any other EVM wallet. They are checked and turned into a key in this tab. Nothing but a signature leaves the browser.", "여기서 만든 것이든 다른 EVM 지갑의 것이든 BIP-39 열두 단어. 이 탭 안에서 확인되고 키로 변환됩니다. 브라우저를 떠나는 것은 서명뿐입니다."),
    ("Signing proves you hold the key for this address. It costs nothing, moves nothing and authorises nothing except this login.", "서명은 이 주소의 키를 보유했음을 증명합니다. 비용도, 자금 이동도 없으며 이 로그인 외에는 아무것도 허가하지 않습니다."),
    ("Method", "방식"),
    ("Nonce expires", "논스 만료"),
    ("Session length", "세션 길이"),
    ("24 hours", "24시간"),
    // ---- menu panel
    ("Every screen in the exchange", "거래소의 모든 화면"),
    ("Claim test money", "테스트 자금 받기"),
    ("Buy", "매수"),
    ("Sell", "매도"),
    ("Open the ticket on the buy side", "매수 쪽으로 주문표 열기"),
    ("Open the ticket on the sell side", "매도 쪽으로 주문표 열기"),
    ("Spot, perpetuals and futures", "현물, 무기한, 선물"),
    ("Positions, equity and risk", "포지션, 자산, 리스크"),
    ("Who is winning", "누가 이기고 있나"),
    ("Chain, RPC and transactions", "체인, RPC, 트랜잭션"),
    ("Your addresses and sessions", "내 주소와 세션"),
    ("Simulation switch and operator controls", "시뮬레이션 스위치와 운영자 제어"),
    // ---- rpc panel
    ("RPC", "RPC"),
    ("Point any client at the same stack", "어떤 클라이언트든 같은 스택에 연결하세요"),
    ("Gateway API", "게이트웨이 API"),
    ("Gateway WebSocket", "게이트웨이 웹소켓"),
    ("Gateway listens on", "게이트웨이 주소"),
    ("Node RPC (Cap'n Proto)", "노드 RPC (Cap'n Proto)"),
    ("Oracle control", "오라클 제어"),
    ("Chain id", "체인 ID"),
    ("Simulation switch", "시뮬레이션 스위치"),
    ("not configured", "설정되지 않음"),
    // ---- dashboard
    ("Chain", "체인"),
    ("Synced", "동기화됨"),
    ("Not synced", "동기화 안 됨"),
    ("Block height", "블록 높이"),
    ("Throughput, 1 min", "처리량, 1분"),
    ("Transactions", "트랜잭션"),
    ("Recent blocks", "최근 블록"),
    ("Recent transactions", "최근 트랜잭션"),
    ("What the chain applied, newest first, from the gateway's block stream", "체인이 적용한 것, 최신순, 게이트웨이 블록 스트림 기준"),
    ("No transactions yet", "아직 트랜잭션 없음"),
    ("Kind", "종류"),
    ("Market", "시장"),
    ("Side", "방향"),
    ("Sender", "발신자"),
    ("Hash", "해시"),
    ("Age", "경과"),
    ("Proposer", "제안자"),
    ("Last 24 blocks, by transaction count", "최근 24개 블록, 트랜잭션 수 기준"),
    ("Copy hash", "해시 복사"),
    ("Hash copied", "해시 복사됨"),
    // ---- faucet
    ("Claim grant", "지급 받기"),
    ("Sign in to claim", "받으려면 로그인"),
    ("Ready to claim", "받을 수 있음"),
    ("On cooldown", "쿨다운 중"),
    ("Next grant", "다음 지급"),
    ("Cooldown", "쿨다운"),
    ("Grant history", "지급 내역"),
    ("Grant claimed", "지급 완료"),
    ("Claims made", "받은 횟수"),
    // ---- ticket
    ("Order ticket", "주문표"),
    ("Buy / Long", "매수 / 롱"),
    ("Sell / Short", "매도 / 숏"),
    ("Limit", "지정가"),
    ("Limit price", "지정 가격"),
    ("Quantity", "수량"),
    ("Post only", "포스트 온리"),
    ("Reduce only", "리듀스 온리"),
    ("Order value", "주문 금액"),
    ("Available margin", "가용 마진"),
    ("Close", "닫기"),
    // ---- account
    ("Profile", "프로필"),
    ("Edit handle", "닉네임 편집"),
    ("Your two addresses", "내 두 주소"),
    ("Both of them are yours", "둘 다 당신 것입니다"),
    ("Login address", "로그인 주소"),
    ("Trading address", "거래 주소"),
    ("You hold the key", "키는 당신이 보유"),
    ("The gateway holds the key", "키는 게이트웨이가 보유"),
    ("Statistics", "통계"),
    ("Only the handle can be changed; the address is the identity", "닉네임만 바꿀 수 있습니다. 주소가 곧 신원입니다"),
    ("Derived from your twelve words. This is the address you signed in with. The gateway has never seen your phrase and cannot sign anything as you.", "열두 단어에서 유도된 주소이며, 로그인에 사용한 주소입니다. 게이트웨이는 문구를 본 적이 없고 당신 대신 서명할 수 없습니다."),
    ("computed from", "왼쪽 주소에서"),
    ("the one on the left", "계산됨"),
    ("The on-chain account holding your test money. The gateway signs your orders with it, which is why placing a trade is one click and not a wallet prompt.", "테스트 자금을 보유한 온체인 계정입니다. 게이트웨이가 이 키로 주문에 서명하므로, 거래가 지갑 확인 없이 한 번의 클릭으로 끝납니다."),
    ("Active sessions", "활성 세션"),
    ("Nobody is signed in", "로그인한 사람이 없습니다"),
    ("Operator", "운영자"),
    ("User", "사용자"),
    // ---- admin
    ("Overview", "개요"),
    ("Simulation", "시뮬레이션"),
    ("Scenario", "시나리오"),
    ("SIMULATION OFF", "시뮬레이션 끔"),
    ("SIMULATION ON", "시뮬레이션 켬"),
    ("MAX SIMULATION", "최대 시뮬레이션"),
    ("No default buyer or seller. The books only move when a person trades.", "기본 매수자·매도자가 없습니다. 사람이 거래할 때만 주문장이 움직입니다."),
    ("A default buyer and seller: one worker, eight traders, about twenty signed orders a second.", "기본 매수자와 매도자: 워커 1개, 트레이더 8명, 초당 약 20건의 서명된 주문."),
    ("Maximum stress: six workers, sixty traders, tens of thousands of signed orders a second.", "최대 부하: 워커 6개, 트레이더 60명, 초당 수만 건의 서명된 주문."),
    ("This is a simulator, so the operator may change the rules of the game. The switch decides who else is trading; the scenario decides what the oracle does to every derivative market's price.", "이것은 시뮬레이터이므로 운영자가 게임의 규칙을 바꿀 수 있습니다. 스위치는 누가 함께 거래하는지를, 시나리오는 오라클이 모든 파생 시장의 가격에 무엇을 하는지를 정합니다."),
    ("Elapsed", "경과 시간"),
    ("Transactions since switched on", "켠 뒤 트랜잭션"),
    ("Seed", "시드"),
    ("Invariant violations", "불변식 위반"),
    ("Calm", "평온"),
    ("Trend up", "상승 추세"),
    ("Trend down", "하락 추세"),
    ("Crash", "폭락"),
    ("Squeeze", "스퀴즈"),
    ("Chop", "횡보"),
    ("Users", "사용자"),
    ("Audit trail", "감사 기록"),
    ("This screen needs an operator session", "이 화면은 운영자 세션이 필요합니다"),
    ("Go to the terminal", "터미널로 가기"),
    // ---- states
    ("Loading", "불러오는 중"),
    ("Connecting", "연결 중"),
    ("Running", "실행 중"),
    ("Stopped", "정지"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_identity_and_korean_falls_back_to_it() {
        assert_eq!(ko("Trade"), Some("거래"));
        assert_eq!(ko("a string nobody translated"), None);
        assert_eq!(Lang::parse("KO"), Some(Lang::Ko));
        assert_eq!(Lang::parse("fr"), None);
    }

    #[test]
    fn every_key_is_unique_and_non_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for (en, ko) in KO {
            assert!(!en.is_empty() && !ko.is_empty(), "{en:?} -> {ko:?}");
            assert!(seen.insert(*en), "duplicate key {en:?}");
        }
    }
}
