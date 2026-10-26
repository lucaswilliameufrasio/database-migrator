# ADR-003: Lock e transações do driver SQLite

- **Status:** Aceita para o primeiro incremento (SQL + SQLite)
- **Data:** 2026-10-23

## Contexto

SQLite não fornece o mesmo lock de sessão dos servidores do plano. O primeiro
executor precisa serializar escritores sem deixar a execução e o estado em
conexões diferentes.

## Decisão

- Cada operação mutável usa uma conexão SQLite física e uma transação
  `BEGIN IMMEDIATE`; SQL, estado e evento de sucesso usam essa mesma conexão.
- `busy_timeout` é configurável e, por padrão, 30 segundos. `SQLITE_BUSY` não
  dispara retry automático da migration.
- A V0 aceita somente migrations transacionais. `transaction=never`, SQL que
  tenta controlar transações/savepoints, `ATTACH`/`DETACH` e `PRAGMA` em
  migrations são recusados.
- `plan` e `status` abrem alvos existentes em modo read-only. Alvo inexistente
  é tratado como vazio sem criar arquivo.
- Mutações são permitidas somente para targets explicitamente marcados como
  `development` ou `test`; produção não é suportada nesta fatia.

## Consequências

O primeiro incremento é deliberadamente limitado a bancos SQLite locais e não
implementa execução concorrente por namespaces, operações não transacionais ou
recuperação de DDL indeterminado.

## Evidências

Testes usam arquivos SQLite reais temporários para provar read-only, rollback
transacional, bloqueio de um segundo writer sem retry automático, execução de
trigger com semicolons internos e recusa de comandos de controle.
