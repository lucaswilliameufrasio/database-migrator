# ADR-020: Metadata v5 para repeatables e seleção de namespace

- **Status:** Aceita para F9
- **Data:** 2026-10-25

## Contexto

O roadmap F9 exige migrations repeatable com identidade por nome lógico e uma
execução auditada a cada alteração de conteúdo. Metadata v4 representa somente
`versioned` e `legacy`, limita `native_version` a timestamp de 20 dígitos e não
aceita direção `repeat` nos runs. Reutilizar uma linha versionada faria uma
alteração de repeatable parecer divergência irreparável; reaproveitar a tabela
legada violaria a proveniência e as regras de adoção de F8. O roadmap também
define namespace como filtro/identidade lógica, sem presumir isolamento físico.

## Decisão

- Evoluir o contrato para metadata v5 e oferecer upgrade **explícito** de v4
  para v5 nos quatro adapters. Leitura e `plan` continuam read-only; qualquer
  escrita em metadata v4 que precise da nova semântica falha indicando o
  upgrade. Upgrades anteriores ainda suportados permanecem explícitos e
  preservam linhas, runs, steps, proveniência e identidade de instalação.
- Uma migration repeatable é um arquivo SQL `R__<name>.sql`. O nome lógico é
  estável dentro do namespace, validado e independente do checksum; colisão de
  nomes no namespace é erro antes da conexão. O `migration_key` inclui namespace
  e o discriminante `repeatable`, sem alterar as chaves de migrations
  versionadas já persistidas.
- `schema_migrations` guarda uma única linha de estado corrente por
  `(namespace, repeatable_name)`, incluindo o último checksum executado. Cada
  conteúdo novo gera run adicional `direction=repeat`; execução de SQL utiliza
  o plano Up e seus steps continuam identificados como Up. O histórico dos runs
  não é sobrescrito quando a linha de estado corrente é atualizada.
- Repeatables são planejadas e executadas depois de todas as versionadas,
  ordenadas por caminho lógico da fonte configurada e nome lógico, sem depender
  da ordem retornada pelo filesystem. Executam quando ausentes ou quando o
  checksum muda. Não recebem Down automático; `down`/rollback de versionadas não
  executam repeatables.
- `--namespace` escolhe um namespace por invocação, sobrescrevendo somente o
  valor configurado em memória. A seleção não cria isolamento de schema,
  permissões ou lock; continua valendo o lock conservador por instalação. Não
  existe seleção implícita de todos os namespaces/tenants.

## Alternativas consideradas

- **Manter metadata v4 e codificar repeatables como versionadas:** rejeitada
  porque a identidade por timestamp e a divergência de checksum não modelam
  reexecução nem preservam runs.
- **Criar uma tabela paralela exclusiva de repeatables sem evoluir o formato:**
  rejeitada por criar duas fontes de verdade para status, planner e histórico, e
  por contrariar o contrato de evolução versionada definido no ADR-019.
- **Tratar mudança de checksum como Down+Up:** rejeitada porque repeatables não
  têm rollback seguro garantido; a alteração é uma nova execução Up auditada.
- **Executar em paralelo por namespace:** rejeitada para V1; namespaces não
  comprovam isolamento físico nem ausência de conflito entre objetos.

## Consequências e evidência necessária

- DDL, codecs, leitura read-only de metadata v4 e upgrade v4→v5 devem ser
  equivalentes em SQLite, PostgreSQL, MySQL e MariaDB; upgrades nos bancos com
  DDL de commit implícito falham fechados em interrupção.
- `validate`, `plan`, `status`, `up`, `down`, `history`, `doctor`, runner e
  JSON devem distinguir repeatables de versionadas sem executar uma repeatable
  em rollback, reiniciar conteúdo inalterado ou perder runs anteriores.
- Testes devem cobrir ordenação determinística entre múltiplas fontes, colisão
  de nomes, namespaces isolados, drift e mudança de checksum, upgrade que
  preserva metadata v4 e repetição/concorrência.
- Revisitar esta decisão se a V1 introduzir dependências explícitas entre
  repeatables/versionadas ou locks comprovadamente isolados por schema/tenant.
