# digest-rs: digest(Node)의 Rust 재작성

School-of-Company/Expo-Monitoring의 `digest/`(Node 22 ESM)를 기능·출력·동작 100% 동일하게 Rust로 옮겨 Node 버전을 대체한다. **개발 우선**: 구현은 Claude가 하고, 끝난 뒤 핵심 Rust 개념만 JS/TS와 비교해 짧게 설명한다.

## 규칙
- 모호한 동작은 Node 코드가 정답이다. 스스로 "개선"하지 말고 개선 아이디어는 목록으로만 제안한다.
- 환경변수 이름·기본값, CLI(`once daily|weekly [--dry-run]`, `serve`), 종료 코드(사용법 2, 실패 1), 로그·사용자 문자열(한국어 포함)을 글자 하나까지 동일하게 유지한다.
- 프로덕션 코드에 `unwrap`/`expect`/`panic!` 금지(테스트 제외). `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` 통과.
- 의존성은 필요한 것만, 추가할 때 이유를 말한다. CLI는 Node usage와 같게 하려고 clap 없이 직접 파싱한다.
- 모듈 단위로 구현·테스트·커밋(`feat(digest-rs): config`). 단계마다 PR 하나. push와 PR은 사용자가 요청할 때만.
- 원본은 이 레포에 없다. 기능 질문은 Expo-Monitoring의 `digest/*.mjs`, `docs/digest.md`를 근거로 답한다.

## 단계
1. config 2. mask 3. schedule 4. http 5. 파싱(prometheus·loki·grafana) 6. report(서식·judge) 7. 수집(`tokio::join!`, HTTP 트레이트) 8. discord 9. main(once/serve) 10. parity 스크립트·Dockerfile·compose·CI

## parity
- Node는 수정하지 않는다. parity 스크립트는 Expo-Monitoring 경로를 받아(로컬은 환경변수, CI는 두 번째 checkout) 같은 가짜 서버로 Node와 Rust를 실행해 비교한다.
- Node가 Prometheus `time` 파라미터로 남긴 실행 시각을 읽어 Rust의 테스트용 `DIGEST_NOW_MS`(once만)로 넘기고, 정규화 없이 바이트 단위로 비교한다 (`dev/verify-digest-parity.mjs`).
- 배포 파일: `digest-rs/Dockerfile`, 루트 `docker-compose.digest-rs.yml`, `.github/workflows/ci.yml`의 `parity`·`docker` job, `digest-rs/README.md`.

## 이식 함정 (반드시 처리)
- 문자열 길이·자르기는 UTF-16 코드 유닛 기준. 이모지가 반으로 잘리면 JSON(dry-run·전송 모두)은 `\udXXX` 이스케이프(`discord::to_js_json`), stdout에 그대로 쓰는 마크다운은 U+FFFD로 Node와 동일하게.
- `toFixed`/`Math.round` 반올림, `Intl.NumberFormat('en-US')`, JS `Number()`·`String(number)` 규칙.
- Rust regex는 lookbehind 미지원, `\b`·`\d`·`(?i)`는 기본이 유니코드 → ASCII로 고정.
- `nextRun`은 chrono-tz로 UTC→로컬만 구하고 JS의 2회 보정 알고리즘을 그대로 옮긴다(DST 결과 동일).
- `URLSearchParams`(공백 `+`), `encodeURIComponent` 인코딩.
- `settle` → `enum Collected<T> { Ok(T), Failed(String), Skipped }`.
