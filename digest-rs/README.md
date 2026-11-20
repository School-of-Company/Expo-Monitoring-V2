# digest-rs

[Expo-Monitoring](https://github.com/School-of-Company/Expo-Monitoring)의 `digest/`(Node 22)를 Rust로 옮긴 일일·주간 요약 도구입니다. Prometheus·Grafana·Loki를 읽어 Discord 웹훅으로 보냅니다. 무엇을 보내는지, 판정 기준, 개인정보 처리, 장애 시 동작은 Node 버전과 같으므로 원본 [`docs/digest.md`](https://github.com/School-of-Company/Expo-Monitoring/blob/main/docs/digest.md)를 따릅니다.

환경변수 이름·기본값, CLI, 종료 코드(사용법 오류 2, 실패 1), 로그·메시지 문구가 Node 버전과 글자 단위로 같습니다. `DIGEST_NOW_MS` 하나만 추가됐고, 동등성 검증용(`once`에서만 실행 시각을 고정)입니다.

## 실행

```bash
cd digest-rs
cargo run -- once daily --dry-run   # 전송 없이 JSON payload + 마크다운 미리보기 (스냅샷 생성 생략)
cargo run -- once weekly            # 한 번 전송
cargo run -- serve                  # 스케줄러 (기본 매일 09:00, 매주 MON 09:00, Asia/Seoul)
cargo test                          # 단위 테스트 (원본 *.test.mjs 이식 포함)
```

컨테이너는 실행 중인 Expo-Monitoring 스택의 네트워크(`expo-monitoring_monitoring`, `MONITORING_NETWORK`로 변경)에 붙습니다. 환경변수는 이 레포 루트의 `.env`에서 읽습니다(원본 `.env.example`의 digest 구역과 같은 이름).

```bash
docker compose -f docker-compose.digest-rs.yml up -d --build digest-rs
```

이미지는 musl 정적 바이너리 + distroless/static(nonroot)입니다. 크기 약 11MB(Node 이미지 약 247MB).

## Node 버전과 동등성 검증

```bash
cargo build
EXPO_MONITORING_DIR=../Expo-Monitoring node ../dev/verify-digest-parity.mjs
```

가짜 Prometheus·Grafana·Loki·Discord에 Node와 Rust를 차례로 실행하고, 종료 코드·stdout·stderr·소스가 받은 요청 전체·Discord payload(multipart는 파트 단위)를 비교합니다. Node가 Prometheus `time` 파라미터로 남긴 실행 시각을 Rust의 `DIGEST_NOW_MS`로 넘기므로 정규화 없이 바이트 단위로 같아야 통과합니다. 시나리오: daily·weekly 전송, dry-run, Prometheus·스냅샷·Loki·Grafana 규칙 500, 패널 일부 실패, Loki·Grafana 미설정, Basic 인증, Discord 429·500·400. CI(`.github/workflows/ci.yml`의 `parity` job)에서도 실행됩니다.

## 알려진 차이

실제 API에서 나오지 않는 입력이나 드문 오류에서만 다릅니다(코드에 `shortcut:` 주석).

- TLS 실패는 `연결 실패`만 표시(Node는 `연결 실패(CERT_HAS_EXPIRED)`처럼 OpenSSL 코드), DNS 일시 실패도 항상 `ENOTFOUND`
- JSON에 짝 없는 서로게이트 이스케이프나 `1e400` 같은 수가 있으면 Node는 받아들이고 Rust는 `응답 형식 오류`
- 배포 주석 `time`이 문자열이면 RFC 3339만 해석
- 응답 배열 자리에 문자열이 오면 JS는 글자 단위로 순회하지만 Rust는 TypeError(`연결 실패`)로 처리
- 시간대 데이터 출처가 다름(Node는 ICU, Rust는 chrono-tz 내장)
