# ADR-002: Metadados iniciais para SQLite

- **Status:** Aceita para o primeiro incremento (SQL + SQLite)
- **Data:** 2026-10-23

## Contexto

O modelo final do plano possui cinco tabelas operacionais. O primeiro corte
suporta apenas migrations SQL SQLite transacionais, mas precisa preservar o
estado atual e o histórico de aplicações e rollbacks.

## Decisão

- O formato inicial cria `schema_migration_meta`, `schema_migrations` e
  `schema_migration_runs`, dentro de transação SQLite.
- A chave corrente é `(namespace, migration_key)`; `migration_key` é SHA-256
  de descriptor nativo versionado. A versão, filename, source path e checksum
  completos também são persistidos.
- `schema_migration_runs` é append-only semanticamente e registra operações
  concluídas. `down` remove o estado corrente apenas na mesma transação que
  grava o evento de rollback.
- `schema_migration_steps` e `schema_migration_locks` ficam para a fase de
  journal/recovery. Este primeiro formato não é declarado como metadata V1
  estável e não faz upgrade automático.
- Bootstrap é permitido somente em comando mutável explícito e sob
  `BEGIN IMMEDIATE`. `plan`, `status`, `history` e `validate` nunca inicializam
  metadados.

## Consequências

O primeiro corte oferece atomicidade por migration em SQLite, não journal de
steps nem reconciliação de estados não transacionais. A futura expansão do
schema precisa de plano/versionamento explícito antes de uso estável.

## Evidências

Testes verificam atomicidade do estado e do histórico, ausência de bootstrap
em consultas read-only e preservação do evento de rollback.
