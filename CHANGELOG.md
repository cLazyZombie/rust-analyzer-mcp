# CHANGELOG

이 문서는 `upstream/main` 포크 기준점 `ce8d64c` 이후의 변경사항을 의도 중심으로 정리한다.
기준 기간은 2025-11-09부터 2026-02-20(현재 워킹트리 미커밋 변경 포함)까지다.

## 2026-02-20 (Working Tree, Uncommitted)

### Diagnostics 갱신을 `set_workspace` 재시작 없이 반영
- 의도: 파일 내용이 바뀌어도 rust-analyzer 진단이 즉시 갱신되도록 만들어, `rust_analyzer_set_workspace`를 반복 호출해야 하는 운영 부담을 제거한다.
- 핵심 변경: 열린 문서 상태를 `HashSet<String>`에서 `HashMap<String, OpenDocumentState { version, content }>`로 바꿔 변경 감지 시 `didChange`를 보내고, 이후 `didSave`로 check-on-save 진단을 트리거하도록 조정했다.
- 핵심 변경: 문서 재오픈이 아닌 증분 동기화 흐름을 사용하면서 URI별 stale diagnostics를 미리 제거해, 호출 시점에 과거 오류가 섞여 보이는 문제를 줄였다.
- 효과: 같은 파일에서 오류 주입/원복을 반복해도 서버 재시작 없이 diagnostics가 나타나고 사라진다.
- 관련 파일: `src/lsp/client.rs`, `tests/integration/document_sync.rs`, `tests/integration_tests.rs`

### `workspace_diagnostics` 응답 정규화/폴백 강화
- 의도: rust-analyzer 버전/설정별로 달라지는 `workspace/diagnostic` 응답 형태 때문에 도구 출력이 불안정해지는 문제를 줄인다.
- 핵심 변경: initialize 응답의 capability를 읽어 `workspace_diagnostics_supported`를 저장하고, 미지원이면 즉시 폴백 경로를 타도록 했다.
- 핵심 변경: pull-diagnostics 응답(`items` 배열)과 URI 맵 형태를 모두 정규화하는 로직을 추가했다.
- 핵심 변경: 폴백 시 캐시된 `publishDiagnostics`가 없으면 워크스페이스의 Rust 파일(최대 128개, `target/.git/node_modules/.idea/.vscode` 제외)을 열어 진단을 유도하도록 했다.
- 효과: `workspace_diagnostics`가 "unexpected format"에 빠지지 않고 일관된 파일별 맵 구조를 반환한다.
- 관련 파일: `src/lsp/client.rs`, `src/lsp/handlers.rs`

### MCP 레이어의 Workspace 진단 포맷 일관화
- 의도: LSP에서 들어오는 severity 타입(숫자/문자열) 차이로 요약 집계가 틀어지는 문제를 막는다.
- 핵심 변경: `format_workspace_diagnostics`가 `items` 기반 리포트와 URI 맵 둘 다 처리하도록 확장했다.
- 핵심 변경: severity 문자열(`error`, `warning`, `information`, `hint`)을 숫자 레벨로 정규화해 합산하고, 파일/전체 요약 구조를 고정했다.
- 효과: `files + summary(total_*)` 형식이 안정적으로 유지된다.
- 관련 파일: `src/mcp/handlers.rs`

### 통합 테스트 신뢰성 강화
- 의도: 비동기 진단 특성 때문에 생기던 flaky 테스트를 줄이고, 기대 출력 형식을 명시적으로 고정한다.
- 핵심 변경: `test_workspace_diagnostics`에서 요약 수치가 실제로 생길 때까지 polling하고, `summary.total_*` 필드를 강제 검증하도록 바꿨다.
- 핵심 변경: 재시작 없이 diagnostics가 갱신되는 시나리오를 별도 테스트(`test_diagnostics_refresh_without_workspace_restart`)로 추가했다.
- 효과: diagnostics 회귀를 CI에서 더 빨리 감지할 수 있다.
- 관련 파일: `tests/integration/diagnostics.rs`, `tests/integration/document_sync.rs`, `tests/integration_tests.rs`

### 에이전트 작업 문서 추가
- 의도: 코드 에이전트가 프로젝트 구조와 수정 규칙을 빠르게 파악해 일관된 수정/테스트를 수행하도록 한다.
- 핵심 변경: 아키텍처 맵, 요청 흐름, 도구 표면, 테스트 레이아웃, 변경 가이드, 실무 가드레일을 포함한 작업 지침 문서를 작성했다.
- 효과: 신규 작업 진입 비용과 실수 가능성을 낮춘다.
- 관련 파일: `AGENTS.md`

## 2026-02-20

### `103680e` - MCP stdio framing Codex 호환성 수정
- 의도: NDJSON만 가정한 파서로는 일부 MCP 클라이언트(Codex)의 `Content-Length` 프레이밍과 충돌할 수 있어 상호운용성을 확보한다.
- 핵심 변경: `StdioTransport`를 도입해 NDJSON과 `Content-Length` 프레이밍을 모두 파싱/응답할 수 있게 만들었다.
- 핵심 변경: 요청 프레이밍 타입을 보존해 동일 프레이밍으로 응답하고, EOF/부분 프레임/헤더 파싱 예외를 분리 처리했다.
- 핵심 변경: initialize 응답에서 클라이언트 프로토콜 버전을 반영하고 서버 버전을 crate version으로 통일했다.
- 핵심 변경: Codex CLI 등록 방법을 README에 문서화하고 transport 단위 테스트/서버 통합 테스트를 추가했다.
- 효과: Codex 환경에서 메시지 경계 오류 없이 요청/응답이 지속적으로 처리된다.
- 관련 파일: `src/mcp/transport.rs`, `src/mcp/server.rs`, `src/mcp/mod.rs`, `README.md`, `test-support/src/test_client.rs`, `tests/stress/concurrent_requests.rs`

## 2025-12-03

### `c0ce8ce` - 전체 요청 디버그 로깅 및 알림 처리 안정화
- 의도: 현장 장애 시 실제 요청 payload를 바로 확인하고, JSON-RPC notification에 잘못 응답해 클라이언트가 깨지는 문제를 예방한다.
- 핵심 변경: 수신 요청 전체를 debug 레벨로 출력하도록 로깅을 보강했다.
- 핵심 변경: `id`가 없는 notification은 응답을 쓰지 않도록 루프 처리 경로를 명확히 분기했다.
- 효과: 디버깅 관측성이 올라가고 notification 관련 상호운용성 문제가 줄었다.
- 관련 파일: `src/mcp/server.rs`

### `f0f287b` - notification 처리 후속 보강
- 의도: notification 처리의 안정성을 높이고 dispatch 단계에서 요청 추적 가능성을 개선한다.
- 핵심 변경: 요청 처리 함수 진입 시점에도 전체 요청 debug 로그를 남기도록 보강했다.
- 효과: run loop와 dispatcher 양쪽 로그를 대조해 원인 파악이 쉬워졌다.
- 관련 파일: `src/mcp/server.rs`

## 2025-11-22

### `ddccca2` - MCP `ping` 지원 및 통합 테스트 추가
- 의도: MCP 기본 유틸리티 스펙(`ping`)을 충족하고 헬스체크용 경량 호출을 제공한다.
- 핵심 변경: `ping` 메서드를 구현해 스펙대로 빈 객체 `{}`를 반환하도록 했다.
- 핵심 변경: IPC 테스트 서버에서 `accept` 후 연결 stream을 blocking 모드로 되돌려 연속 요청 시 `EAGAIN/EWOULDBLOCK`을 방지했다.
- 핵심 변경: ping 응답 형식/연속 호출/고속 호출/호출 후 서버 생존성까지 검증하는 통합 테스트를 추가했다.
- 효과: 클라이언트 헬스체크와 테스트 인프라 안정성이 함께 개선됐다.
- 관련 파일: `src/mcp/server.rs`, `test-support/src/ipc/server.rs`, `tests/integration/mcp_server_test.rs`

### `dc6afae` - IPC 테스트 인프라 경로 안정화
- 의도: OS/환경별 경로 이슈로 인한 테스트 실패를 줄인다.
- 핵심 변경: 워크스페이스 경로를 canonicalize해 절대 경로 기준으로 일관성 있게 동작하도록 했다.
- 핵심 변경: macOS `SUN_LEN` 제한을 피하기 위해 소켓 경로/이름을 단축했다.
- 효과: 진단 관련 통합 테스트의 실패 원인을 기능 코드가 아닌 인프라에서 제거했다.
- 관련 파일: `test-support/src/ipc/client.rs`, `test-support/src/ipc/server.rs`

### `3283d9a` - ping 스트레스 테스트 확장
- 의도: `ping` 추가 이후 실제 부하 패턴(연속/동시/혼합/버스트/연결 재생성)에서도 서버가 안정적인지 검증한다.
- 핵심 변경: 6개 시나리오(rapid fire, concurrent, tools interleave, mixed workload, burst, connection stability)를 추가했다.
- 핵심 변경: CI 환경을 고려한 반복 수 조절과 스펙 준수(assert `{}`)를 포함했다.
- 효과: 고빈도 헬스체크 상황에서의 회귀를 조기에 잡을 수 있게 됐다.
- 관련 파일: `tests/stress/concurrent_requests.rs`

## 2025-11-09

### `f1b8d51` - 서버 실행 경로를 generic stream으로 리팩터링
- 의도: stdio 고정 실행 방식에서 벗어나 라이브러리 임베딩/테스트 더블/커스텀 IO 환경을 지원한다.
- 핵심 변경: `run_with_streams<R, W>`를 추가해 `AsyncRead/AsyncWrite` 제네릭 스트림을 받을 수 있게 했다.
- 핵심 변경: 기존 `run()`은 유지하되 내부에서 새 경로로 위임해 하위 호환성을 보존했다.
- 효과: 테스트 용이성과 서버 재사용성이 높아졌다.
- 관련 파일: `src/mcp/server.rs`
