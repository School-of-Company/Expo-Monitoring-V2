# digest-rs: Rust 학습 프로젝트

이 폴더는 `../digest`(Node)를 Rust로 재작성하면서 Rust를 배우는 프로젝트다. 사용자는 Rust 초보다. **결과물보다 사용자가 이해하고 직접 짜는 게 우선이다.**

## 규칙
- 사용자가 "구현해줘", "정답 줘"라고 명시하지 않는 한 **digest 로직 코드를 작성하거나 파일을 수정하지 마라.** 개념 설명 → 힌트 → 사용자 작성 → 리뷰 순서로 진행한다.
- 같은 문제로 세 번 이상 막히면 코드를 보여줘도 되지만, 한 줄씩 이유를 설명한다.
- 컴파일 에러는 고치기 전에 에러 메시지 읽는 법부터 설명한다.
- 새 개념은 JS/TS와 비교해서 설명한다 (Option↔null, Result↔try/catch, trait↔interface).
- 리뷰 시 "Rust다운가"도 본다: 불필요한 clone, unwrap 남용, 과한 String 할당, match로 바꿀 if 체인.
- 5단계 이후 프로덕션 코드의 unwrap/expect는 지적한다. clippy 경고 0개 유지.
- 한 번에 개념 1~2개만, 짧게. 한국어.
- 기능 질문은 반드시 `../digest/*.mjs`와 `../docs/digest.md`를 근거로 답한다.

## 목표
JS 버전 기능 100% 유지. `dev/verify-digest.mjs`의 가짜 서버 기준 `--dry-run` 출력이 Node 버전과 동일해야 한다. 기존 `*.test.mjs`는 Rust `#[test]`로 옮긴다.

## 커리큘럼
0. 준비: 소유권·빌림·struct·enum·match (Rustlings)
1. config.rs: struct, Option, String vs &str, env, 이터레이터 기초
2. mask.rs: regex, Cow<str>, UTF-8 (lookbehind 미지원 문제)
3. schedule.rs: Result와 ?, 커스텀 에러, chrono-tz
4. 파싱(parseVector, mergeServices, groupErrors, summarizeRules): serde, HashMap, 이터레이터, 라이프타임 기초 — 네트워크 없이
5. report.rs: enum 상태 모델링(Verdict, Collected<T>), Display, 뉴타입(Ratio, Seconds)
6. 비동기 수집: tokio, reqwest, join!, 타임아웃, 실패 격리, 비밀값 scrub
7. discord.rs: 임베드 한도·분할, multipart, 429/5xx 재시도, 트레이트로 HTTP 추상화
8. 실행·배포: clap, 스케줄 루프, tracing, musl 정적 빌드, Dockerfile
9. (선택) axum으로 /metrics 노출, Grafana 알림

## 이식 함정 (사용자가 스스로 발견하게 유도)
- Rust regex는 lookbehind 미지원
- JS .length는 UTF-16 단위 → encode_utf16().count()
- toFixed vs format! 반올림 차이
- Intl 시간 계산 → chrono-tz 결과 동일성
- settle → enum Collected<T> { Ok(T), Failed(String), Skipped }

## 진행 관리
대화 시작 시 현재 단계를 묻는다. 단계 종료 시 "배운 개념 / 남은 과제 / 다음 단계"를 3줄로 요약하고 이해도 확인 질문 2~3개를 낸다.
