# ADR-017: AST declarativa e compiladores da DSL de schema

- **Status:** Aceita; G6 homologado localmente em 2026-10-24; API provisória
- **Data:** 2026-10-24

## Contexto

F6 introduz operações de schema declarativas sobre os quatro dialects já
exercitados, sem transformar SQL concatenado ou closures imperativas em um plano
que pareça verificável. A estrutura de crates continua deliberadamente adiada
pela ADR-014. O plano de desenvolvimento define a experiência e as garantias,
mas deixa as assinaturas Rust orientativas; G6 precisa demonstrar AST imutável,
compilação por dialect, erros de capability e `Down` declarado.

## Decisão

- Implementar a DSL como módulo público `dsl` no crate existente. Considerar
  extração para crate próprio somente depois de evidência do contrato e do
  runner, sem modularização antecipada.
- Separar um builder mutável de um `SchemaPlan` selado: depois de `finish`, o
  plano expõe somente referências imutáveis às operações tipadas.
- Representar migrations declarativas com planos `up` e `down` fornecidos
  separadamente. Não sintetizar `Down` por inversão de AST.
- Compilar offline e sem conexão a banco. Cada operação tipada tem compilação
  determinística por SQLite, PostgreSQL ou MySQL/MariaDB; incompatibilidades de
  semântica retornam `Unsupported` antes de qualquer execução.
- Tipos cuja semântica difere entre servidores exigem uma representação
  explícita (por exemplo, UUID/JSON/timezone); o compilador não escolhe
  silenciosamente uma aproximação.
- `raw_sql` é um escape hatch de statement único, rotulado pelo dialect e pela
  classe de segurança declarada. Não é analisado como operação tipada nem ganha
  guarantees de introspecção por causa do rótulo.
- Fingerprints da AST e do SQL compilado são domínios distintos e versionados;
  ambos incluem o dialect e a versão do compilador quando aplicável.
- F6 produz planos compilados, mas não registra fontes DSL na CLI nem executa
  migrations Rust/DSL. Registro, composição de planos mistos e execução ficam
  para F7/runner.

## Alternativas consideradas

- **Gerar Down automaticamente:** rejeitada porque operações destrutivas ou
  perda de dados não têm inversão segura genérica.
- **Compilar diretamente para strings dentro dos builders:** rejeitada porque
  perderia a AST canônica necessária para validação, capability errors e hash
  lógico independente do SQL gerado.
- **Adicionar um crate `dbm-dsl` agora:** rejeitada pela decisão da ADR-014 de
  manter o crate agrupado até validar as fronteiras com todos os adapters e o
  runner.
- **Executar DSL diretamente na CLI antes do runner:** rejeitada porque F7
  precisa compor SQL e código Rust em uma única sequência sob um único lock e
  lifecycle.

## Consequências e revisitar quando

- A API ainda pode evoluir sem garantia de compatibilidade; não publicar como
  contrato estável antes de G12.
- Testes de unidade cobrem canonicalização/compilação; os planos básicos foram
  executados contra instâncias descartáveis para G6.
- Revisitar a fronteira do módulo ao implementar o runner F7 e qualquer
  capability que dependa da versão/capacidade detectada em runtime.

## Evidência G6

Em 2026-10-24, testes unitários passaram para canonicalização, fingerprints,
validação de identificadores/defaults/constraints, escaping, Up/Down e erros de
capability. As operações `CREATE TABLE`, PK/UNIQUE/CHECK/FK, índice único,
inserts que confirmam as constraints e o Down explícito foram executados em
SQLite, PostgreSQL 16/17/18, MySQL 8.4 e MariaDB 10.11.14/11.4.13/11.8.2. As
execuções externas usaram apenas bases descartáveis em loopback. Isso homologa
somente a fatia declarativa coberta pelos testes; não integra a DSL ao executor,
CLI ou runner e não conclui a V1.
