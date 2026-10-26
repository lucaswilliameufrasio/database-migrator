# ADR-004: Formato SQL inicial

- **Status:** Aceita para o primeiro incremento (SQL + SQLite)
- **Data:** 2026-10-23

## Contexto

O formato nativo precisa representar Up e Down no mesmo arquivo e evitar
divisão ingênua por `;`, especialmente em strings e triggers.

## Decisão

- O formato padrão é um arquivo UTF-8 com diretivas em linhas próprias:
  `-- dbm:dialect sqlite`, `-- dbm:transaction auto|always`, `-- dbm:up` e
  `-- dbm:down` opcional.
- Também são descobertos pares `<version>_<description>.up.sql` e
  `.down.sql`; `.down.sql` sem Up é erro. Single-file e par não podem
  compartilhar versão.
- Marcadores são reconhecidos somente fora de strings e comentários SQL.
  Execução usa `execute_batch` do SQLite dentro da transação gerenciada; não
  existe `split(';')`.
- Um authorizer SQLite impede que o script finalize/abra transações, use
  savepoints, altere tabelas internas, anexe bancos ou altere pragmas.
- Dialetos diferentes de SQLite e `transaction=never` falham antes da
  execução. Um `down` ausente torna a migration irreversível.

## Consequências

O parser inicial valida estrutura e diretivas, mas não substitui parser
semântico SQL nem `dbm test` em banco descartável. Sintaxe SQL inválida pode
ser descoberta apenas na execução.

## Evidências

Testes cobrem diretivas em comentários/strings, semicolons internos, tentativa
de `COMMIT`, falha de statement e rollback completo.
