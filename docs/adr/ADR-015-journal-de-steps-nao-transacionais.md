# ADR-015: Journal para steps PostgreSQL não transacionais

- **Status:** Aceita e implementada para F4; G4 aprovado em 2026-10-24
- **Data:** 2026-10-24

## Contexto

O formato de metadata 2 restringe `schema_migration_steps.transaction_class` a
`transactional`. PostgreSQL tem operações como `CREATE INDEX CONCURRENTLY` que
não podem compartilhar a transação do runner. Registrar intent e confirmação na
mesma transação da DDL não é possível, e falsificar a classe de execução tornaria
o journal enganoso.

## Decisão

- Evoluir o schema lógico para metadata formato 3. SQLite deve oferecer upgrade
  explícito e transacional de formatos 1 e 2, preservando todos os registros;
  PostgreSQL cria formato 3 para novas instalações. Formatos antigos não são
  alterados implicitamente por comandos de leitura.
- O journal distingue `transactional` de `non_transactional`. Nesta fatia
  PostgreSQL, fora de transação só são aceitos `CREATE [UNIQUE] INDEX
  CONCURRENTLY` e `DROP INDEX CONCURRENTLY`; SQLite continua rejeitando
  migrations `never`.
- Para cada statement não transacional, persistir intent e fazer COMMIT, executar
  exatamente um statement fora de uma transação explícita e só então persistir e
  confirmar o ACK em outra transação. Uma falha, perda de resposta ou perda da
  sessão deixa o resultado indeterminado, bloqueia novas mutações e exige
  inspeção/reconciliação explícita; nunca repetir automaticamente.
- Execução PostgreSQL depende de conexão direta ou pooler em modo de sessão. A
  identidade do backend é verificada em cada fronteira; mudança de PID falha
  fechado. A evidência adversarial foi obtida com PgBouncer 1.26.1-p0 em
  transaction pooling, que foi rejeitado quando trocou o backend.
- `up`/`down` PostgreSQL ficam habilitados para alvos não produtivos depois de G4.
  O adaptador aceita apenas índice simples não qualificado com colunas não
  qualificadas; a reconciliação compara catálogo, schema, tabela, colunas,
  unicidade, validade e opções padrão. PostgreSQL metadata 2 pode ser lido, mas
  mutações exigem formato 3; upgrade PostgreSQL não faz parte desta fatia.

## Evidência do gate G4

- Sete testes PostgreSQL opt-in passaram em cada PostgreSQL 16, 17 e 18,
  incluindo leitura de metadata v2/v3, lock pinned, rollback transacional,
  perda de sessão, FSM compartilhada e falha/reconciliação não transacional.
- O fluxo CLI `plan/up/doctor/down/history`, com migration transacional e índice
  concorrente, passou em PostgreSQL 16, 17 e 18.
- O teste TLS local validou uma CA configurada e rejeitou a conexão sem essa CA.
- O teste PgBouncer em transaction pooling confirmou a rejeição da troca de
  backend. Todos os alvos usados foram loopback descartáveis.

## Alternativas consideradas

- **Manter formato 2 e gravar todos os steps como transacionais:** rejeitada por
  registrar incorretamente a garantia de atomicidade.
- **Habilitar SQL não transacional arbitrário:** rejeitada nesta fase por ampliar
  a superfície de operações com efeitos parciais sem capability e recuperação
  específicas.
- **Considerar transaction pooling compatível por manter um `Client` lógico:**
  rejeitada; `Client` não garante o mesmo backend PostgreSQL entre transações do
  PgBouncer.

## Consequências e revisitar quando

- A versão de metadata mudou; SQLite upgrade explícito de formatos 1 e 2 para 3
  preserva registros em transação. PostgreSQL ainda não tem upgrade de formato 2.
- Um resultado não transacional incerto conserva evidência durável e requer
  decisão operacional; não há retry cego.
- Reavaliar a whitelist quando houver introspecção/capability por servidor e
  reconciliação PostgreSQL completa.
