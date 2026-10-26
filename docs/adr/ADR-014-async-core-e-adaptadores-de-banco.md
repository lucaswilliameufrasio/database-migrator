# ADR-014: Núcleo async e adaptadores de banco

- **Status:** Aceita para F4, API interna e provisória
- **Data:** 2026-10-24

## Contexto

A implementação SQLite começou síncrona e em um único crate. F4 adiciona
PostgreSQL via `tokio-postgres`, cuja conexão e lock advisory são ligados à
sessão física e cuja tarefa de I/O precisa ser supervisionada. Um segundo
executor PostgreSQL independente duplicaria FSM, planner, doctor e políticas de
segurança. A ADR-007 deixou a forma final aberta até a PoC de driver.

## Decisão

- Usar uma única orquestração async interna para SQLite e PostgreSQL, mantendo
  contrato ainda não estável nem publicamente extensível nesta fase.
- Separar `DatabaseAdapter` (conexão/capabilities) de `DatabaseSession`
  (sessão física pinned, lock e execução). Futures boxed tornam os traits
  object-safe sem exigir macros de async-trait; detalhes permanecem internos até
  validação com MySQL/MariaDB e o runner.
- A sessão PostgreSQL mantém o mesmo `Client` durante lock, transação, statements
  e atualização de metadata; a tarefa `Connection` é parte do lifecycle da
  sessão. Não buscar conexões novas entre essas etapas nem usar pooler em modo
  transaction pooling. Perda da tarefa/sessão libera o advisory lock no servidor;
  o resultado de COMMIT pode continuar incerto e exige `doctor`, nunca retry
  cego.
- A conexão SQLite fica em um worker blocking dedicado que possui o
  `rusqlite::Connection`; o núcleo async não executa I/O SQLite nem espera
  `busy_timeout` em thread Tokio. O worker serializa comandos e preserva a mesma
  conexão durante transações.
- Estado de sessão, transação e capabilities são explícitos. Cada migration
  transacional grava mudança de schema e sucesso no mesmo COMMIT. Operações
  PostgreSQL fora de transação só serão habilitadas quando o journal conseguir
  persistir intent/ack fora da transação e a sessão pinned permanecer válida.
- `url_env` referencia apenas o nome de uma variável de ambiente; a URL
  resolvida nunca entra em config serializada, argumentos, erros, plano, logs ou
  fingerprint. TLS e validação de certificado são o padrão remoto; plaintext
  fica restrito a configuração de teste/local explicitamente opt-in. Este ADR
  não habilita mutações em produção.
- Manter o crate atual durante a PoC; extrair crates só quando a fronteira tiver
  evidência de SQLite, PostgreSQL e MySQL/MariaDB, evitando modularização
  antecipada.

## Jornadas e lifecycle

1. CLI carrega config tipada e resolve apenas a referência `url_env`.
2. Adapter conecta e informa versão/capabilities; conexão e schema incompatíveis
   falham antes do SQL de migration.
3. Session adquire lock; `up`/`down` revalidam plano e fonte, executam cada
   migration conforme suas capabilities e persistem runs/steps.
4. Conclusão confirma estado e libera lock na própria sessão; perda de conexão
   deixa estado diagnóstico para `doctor`/reconciliação, sem assumir sucesso.
5. `plan`, `status`, `history` e `doctor` sem o subcomando `reconcile` permanecem
   read-only; não criam nem atualizam metadata implicitamente. A reconciliação é
   uma mutação separada, explícita e auditada.

## Alternativas consideradas

- **Segundo engine PostgreSQL assíncrono ao lado do engine SQLite síncrono:**
  rejeitado por duplicar FSM, regras de recuperação e políticas, permitindo
  divergência entre drivers.
- **Driver síncrono PostgreSQL:** rejeitado porque o roadmap fixa
  `tokio-postgres` e exige observação explícita da tarefa de I/O e da sessão.
- **Pool genérico com conexão potencialmente trocada:** rejeitado porque
  advisory locks são de sessão e precisam cobrir toda a execução.
- **API pública estável antes da matriz de bancos:** rejeitada por congelar
  ownership/capabilities sem evidência MySQL/MariaDB e embedded.

## Consequências e próximos passos

- A aceitação desta ADR fixa a direção. A implementação F4 mantém traits
  internos `DatabaseAdapter`/`DatabaseSession`, um worker blocking dedicado para
  SQLite e conexão PostgreSQL pinned com TLS validado. O CLI usa uma FSM async
  compartilhada para `up`/`down`; reads permanecem sem bootstrap implícito.
- **G4 passou em 2026-10-24:** sete testes de integração opt-in passaram em cada
  PostgreSQL 16, 17 e 18; a suíte incluiu transações, advisory lock, falha de
  sessão, execução e recuperação. Também passaram o teste TLS local com CA
  confiável e rejeição sem essa CA, o teste adversarial com PgBouncer 1.26.1-p0
  em transaction pooling e o fluxo CLI PostgreSQL `plan/up/doctor/down/history`
  nas três versões. Os testes usaram somente servidores loopback descartáveis.
- Metadata PostgreSQL novo usa formato 3. Formatos PostgreSQL 2 e 3 podem ser
  inspecionados, mas mutações exigem formato 3; upgrade PostgreSQL ainda não
  existe. SQLite oferece upgrades explícitos e transacionais de formatos 1 e 2
  para 3. Incompatibilidade ou metadata parcial falha fechado.
- A whitelist PostgreSQL fora de transação continua limitada a um índice simples
  criado com `CREATE [UNIQUE] INDEX CONCURRENTLY` e, opcionalmente, removido no
  Down correspondente. Resultado incerto exige diagnóstico/reconciliação; não
  há retry automático. A inspeção de catálogo valida nome/schema, tabela,
  colunas, unicidade, validade e opções padrão antes de aceitar uma reconciliação.
- **G5 passou em 2026-10-24;** a matriz MySQL/MariaDB, deadlocks e recovery está
  registrada na ADR-016. **G6 também passou em 2026-10-24**, com evidência na
  ADR-017. F7–F12/G7–G12, runner, importadores e os demais entregáveis da V1
  continuam pendentes; a aprovação de G4, G5 ou G6 não representa conclusão da
  V1.

## Revisitar quando

Rever na definição da API embedded/custom driver e ao introduzir capabilities
ou execução transacional para statements atualmente não transacionais.
