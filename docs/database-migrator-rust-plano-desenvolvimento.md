# PLANO MESTRE DE DESENVOLVIMENTO — DATABASE MIGRATOR RUST

> **Estado:** escopo arquitetural aprovado; especificação de implementação V1 → V2 → V3  
> **Data:** 21 de outubro de 2026  
> **Nome do produto / binário:** provisórios; nos exemplos, `dbm`. Confirmar disponibilidade de crates e binário antes de publicar.  
> **Repositório proposto:** `database-migrator`  
> **Linguagem:** Rust, edição estável homologada no início da implementação, com MSRV documentada  
> **Licença proposta:** MIT OR Apache-2.0  
> **Dependências de execução obrigatórias:** nenhuma além do banco de dados de destino, conforme o adaptador (não requer servidor, sidecar, ORM, SQLx, Node nem Go).

## 0. Missão e definição de sucesso

Construir uma biblioteca e CLI para gerenciamento de schema de banco de dados, combinando:

1. ergonomia de criação, edição, batches e extensibilidade do Knex;
2. simplicidade de migrations SQL em um único arquivo e execução programática do Goose;
3. segurança operacional (identidade estável, checksum, locking, journal, doctor, importação legada, observabilidade);
4. integração opcional e consciente de segurança com agentes e modelos de IA.

**A V1 só estará completa se suportar todas as decisões aprovadas:** SQL, migrations Rust e DSL tipada; arquivo SQL único Up/Down com pares opcionais; timestamped UTC por padrão, nomes integrais; `rusqlite`, `tokio-postgres`, `mysql_async` com MySQL e MariaDB homologados; drivers customizados por crates; biblioteca embedded e runner compilado; CLI agent-friendly; BYOK com geração e revisão; importação de Knex/Goose/SQLx; journal por instrução e `doctor` com reconciliação manual de estados indeterminados.

### 0.1 Não objetivos da V1

- ORM, modelagem de entidades ou query builder de aplicação completo.
- Traduzir SQL arbitrário entre dialetos ou prometer portabilidade total de SQL.
- Interpretar, compilar automaticamente ou executar arquivos Rust de terceiros recebidos por prompt.
- Carregar drivers Rust dinamicamente por ABI, WASM ou plugins não confiáveis.
- Serviço centralizado, dashboard SaaS, sidecar, agente sempre ligado ou dependência de nuvem.
- Descrever um `dry-run` como teste de execução: simulação e teste real são operações diferentes.
- Autocorreção de falhas parciais, rollback implícito de código legado ou execução autônoma de IA em produção.
- Garantir que código Rust arbitrário possa ser totalmente inspecionado, sandboxed ou reexecutado sem seu binário original.

### 0.2 Contratos invariantes

- Uma identidade de migration aplicada jamais será reutilizada para conteúdo diferente.
- Um arquivo renomeado ou modificado depois de aplicado deve provocar divergência visível, não uma reaplicação silenciosa.
- Não haverá execução mutável sem lock válido e confirmação de capacidades do driver.
- Plano read-only jamais inicializa tabelas internas, altera flags, toma lock de escrita ou aplica migrations.
- Resultado de execução desconhecido implica falha fechada e bloqueio de novos writes até reconciliação.
- Erros, traces, JSON e prompts nunca expõem credenciais nem dados de aplicação por padrão.
- A tabela de estado não será apagada no rollback sem preservar eventos na trilha de auditoria.
- Versões, nomes e identidades importados permanecem legíveis e sem perdas de representação.

---

## 1. Produto, públicos e jornadas

### 1.1 Públicos

- Engenheiro Rust que deseja iniciar e executar migrations dentro do aplicativo.
- Equipe de plataforma executando mudanças via CI/CD com binário standalone.
- Projeto com migrations SQL versionadas sem dependência de ORM.
- Equipe migrando gradualmente de Knex, Goose ou SQLx.
- Autor de adaptador para um banco ainda não suportado.
- Agente de IA que precisa descobrir schema, gerar um patch e obter validação estruturada antes de solicitar aprovação.

### 1.2 Jornadas V1 obrigatórias

J01. Inicializar projeto, definir driver, connection string secret-backed, namespace e diretório de migrations.  
J02. Criar, editar e validar migration SQL Up/Down em um arquivo.  
J03. Criar arquivo SQL `.up.sql` + `.down.sql` opcional e manter a mesma identidade.  
J04. Criar migration Rust/DSL, registrar no runner, compilar e executá-la sem interpretar código Rust.  
J05. Planejar/validar, aplicar, acompanhar status, histórico, rollback de uma migration e de um batch.  
J06. Descobrir descompasso de checksum, nome, ausência de arquivo, ordem e dialeto.  
J07. Executar migration não transacional com journal por step e diagnosticar falha parcial.  
J08. Importar bancos já gerenciados por Knex, Goose ou SQLx, sem executar migrations herdadas.  
J09. Reaplicar repeatables quando o conteúdo mudar e registrar cada execução.  
J10. Trabalhar com filesystem e fonte embedded, namespaces e runner customizado.  
J11. Gerar/revisar migration com IA opcional, exportar patch, plano, riscos e recomendações.  
J12. Rodar em CI/CD, com JSON estrito e ausência de prompts interativos.  
J13. Criar um adaptador customizado em crate externo e executar o contrato de conformidade.  
J14. Gerar schema snapshot/introspecção read-only com cobertura declarada; distinguir drift de arquivo e de catálogo.

### 1.3 Critérios de produto

- Começar com SQL deve exigir poucos comandos e configuração mínima.
- Toda falha deve dizer o que ocorreu, o que foi alterado com certeza, o que é desconhecido e como continuar com segurança.
- A experiência de custom driver não poderá exigir copiar código privado do executor.
- Operações `up` e `down` precisam ser previsíveis, repetíveis somente quando comprovadamente seguro e auditáveis.

---

## 2. Arquitetura: monorepo Cargo, sem microservices

```text
                        CLI (`dbm`)       Runner compilado          API embedded
                              \                |                  /
                               \               |                 /
                    +----------- Application API -----------+
                    | discovery / validation / plan / policy |
                    +-------------------+------------------+
                                        |
              +-------------------------+-------------------------+
              | Migration Engine / Executor / Recovery            |
              | batches, ordering, capability checks, locks       |
              +-------------------------+-------------------------+
                |             |              |               |
         SQL/parser        DSL/AST         Metadata      IA (opcional)
                |             |              |               |
                +-------------+--------------+---------------+
                                        |
                            DatabaseAdapter API
                                        |
                     +------------------+-------------------+
                     |                  |                   |
                  SQLite           PostgreSQL       MySQL / MariaDB
                  rusqlite       tokio-postgres        mysql_async
```

### 2.1 Estrutura sugerida

```text
database-migrator/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── LICENSE-MIT
├── LICENSE-APACHE
├── crates/
│   ├── dbm-core/                  # ids, modelo, errors, capabilities, policies
│   ├── dbm-engine/                # planner, executor, FSM, batches, recovery
│   ├── dbm-metadata/              # schema versioning, state, runs, steps
│   ├── dbm-sql/                   # directives, scanner, statement boundaries
│   ├── dbm-dsl/                   # AST e compilers de schema
│   ├── dbm-driver-api/            # DatabaseAdapter/Session/Lock/Introspection
│   ├── dbm-driver-sqlite/         # rusqlite + worker isolado
│   ├── dbm-driver-postgres/       # tokio-postgres
│   ├── dbm-driver-mysql/          # mysql_async + dialetos MySQL/MariaDB
│   ├── dbm-source/                # filesystem, embedded, registries
│   ├── dbm-runner/                # library API para runners compilados
│   ├── dbm-import/                # knex, goose, sqlx, validação e cutover
│   ├── dbm-ai/                    # providers, prompts, generation, review
│   ├── dbm-observability/         # tracing, events, metrics hooks
│   └── dbm-cli/                   # clap, JSON output, shell completions
├── examples/
│   ├── sql-only/
│   ├── embedded-rust/
│   ├── typed-dsl/
│   ├── compiled-runner/
│   ├── custom-driver/
│   └── ai-byok/
├── tests/
│   ├── fixtures/
│   ├── conformance/
│   ├── integration/
│   ├── import-legacy/
│   └── fault-injection/
├── benches/
├── docs/
│   ├── architecture/
│   ├── adr/
│   ├── cli/
│   ├── dialects/
│   ├── security/
│   ├── operations/
│   └── migration-format/
└── .github/workflows/
```

A lista define responsabilidades, não obrigação de manter 15 crates pequenos desde o dia 1. Agrupar crates internos durante a V0 é aceitável se as interfaces permanecerem explícitas. Não permitir dependências cíclicas: `core` não conhece drivers; drivers conhecem API; engine conhece abstrações; CLI/runner compõem.

### 2.2 Linguagem e dependências

- Rust estável, Cargo workspace, edição e MSRV congeladas em ADR na abertura da implementação; `Cargo.lock` versionado para binários e exemplos reprodutíveis.
- `tokio`, `clap`, `serde`, `serde_json`, `toml`, `tracing`, `thiserror`, `sha2`, `uuid`, `time` ou equivalente conforme necessidade real.
- Drivers obrigatórios: `rusqlite`, `tokio-postgres`, `mysql_async`, com revisão de TLS, compilação e feature flags.
- `rusqlite` executado num worker dedicado ou pool limitado de bloqueio; nunca bloquear threads core do Tokio.
- Clientes PostgreSQL/MySQL mantêm conexão pinned, com observação da tarefa de I/O e perda de sessão.
- API pública orientada a traits pequenas e `async` object-safe (futures encaixotadas ou adapter explícito); evitar herança de comportamento, macros excessivas e acoplamento a uma versão específica de driver.
- Gerar arquivos com LF e UTF-8 por padrão; sem normalização silenciosa em arquivo já aplicado.

### 2.3 Feature flags e publicação

- `dbm-core`: sem dependência dos drivers.
- `dbm-engine`: engine base com suporte a fontes e adaptadores em runtime.
- `dbm-driver-*`: publicáveis separadamente.
- `dbm-ai`: `feature = ai`; biblioteca principal não exige qualquer SDK de LLM.
- `dbm-cli`: releases em perfis `minimal` (SQL e drivers selecionados) e `full` (drivers padrão + IA), mantendo opção compilável a partir do source.
- Features devem ser aditivas; não alterar semântica de segurança ao habilitá-las.

---

## 3. Identidade, timestamps, arquivos e diretórios

### 3.1 Versões nativas

Versão temporal UTC de **20 dígitos** (`YYYYMMDDHHMMSSffffff`, microssegundos), como:

```text
20261021162643981234_create_users.sql
```

- `version` contém somente o timestamp; `filename` armazena o nome completo com extensão; `description` armazena a descrição; `source_path` contém o caminho lógico normalizado da fonte.
- Ordenar por `version` ascendente, desempate proibido; duplicidade no mesmo namespace/família é erro antes da conexão.
- Usar criação atômica do arquivo (`create_new`) e detectar timestamps concorrentes/reversão de relógio. Nunca sobrescrever; se não houver um timestamp real inequívoco, esperar o relógio avançar ou falhar.
- Migrations importadas conservam versões originais mesmo quando não obedecem ao formato de 20 dígitos.
- Renomear um arquivo nativo aplicado mantém `version`, porém muda `filename`: classificar como divergência em vez de tratá-lo como migration nova. Remover arquivo aplicado também é divergência.

### 3.2 Layout de usuário (convenção proposta)

```text
my-app/
├── dbm.toml
├── migrations/
│   ├── versioned/
│   │   ├── 20261021162643981234_create_users.sql
│   │   ├── 20261021162712000441_add_email_index.up.sql
│   │   └── 20261021162712000441_add_email_index.down.sql
│   └── repeatable/
│       └── R__active_users_view.sql
├── src/db_migrations/
│   ├── mod.rs
│   └── m_20261021162842000001_backfill_users.rs
└── src/bin/dbm_runner.rs
```

- Arquivo SQL single Up/Down é o padrão. Pares são opcionais e **nunca coexistem** com arquivo single da mesma `version`.
- Rust usa nome de módulo válido (`m_...`), mas o identificador canônico registrado permanece o timestamp e o nome lógico integral é preservado.
- Repeatables possuem identidade por nome lógico (prefixo `R__`) e checksum por conteúdo; são executadas depois das versionadas, em ordem lexicográfica determinística, quando dependências e conteúdo mudarem.
- Fontes customizadas expõem `logical_path`, bytes, tipo e origem; o núcleo nunca assume que toda fonte é pasta local.

### 3.3 Hashes e integridade

- Para SQL: SHA-256 dos bytes exatos armazenados, guardando algoritmo e versão da estratégia (`sha256:raw:v1`). Não eliminar comentários, espaços ou CRLF após aplicação. Gerador inicial em LF reduz ruído.
- Guardar opcionalmente hashes separados de Up/Down **e** hash do arquivo inteiro; mudanças apenas no Down também devem ser detectadas.
- Para DSL declarativa: fingerprint versionado do plano/AST canônico, do source registrado e da versão do compilador de dialeto, distinguindo hash lógico do hash dos SQLs gerados.
- Para Rust programático: hash do arquivo/manifest registrado **não prova equivalência do binário**; persistir também fingerprint do binário/build/revisão conhecida quando disponível; sem evidência de build, indicar `unverifiable`, nunca `verified`.
- Importados: guardar hash original e algoritmo original (p.ex. SHA-384 do SQLx) sem recalculá-lo com o algoritmo nativo fingindo equivalência.
- Nenhum ajuste automático de checksum. Para atualizar uma migration já aplicada, criar migration corretiva. Em caso excepcional, ferramenta de reconciliação auditada e explícita, nunca edição silenciosa.

---

## 4. Formato SQL, scanner e parser

### 4.1 Sintaxe nativa V1

```sql
-- dbm:dialect postgres
-- dbm:transaction auto
-- dbm:up
CREATE TABLE users (
    id UUID PRIMARY KEY,
    email VARCHAR(255) NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- dbm:down
DROP TABLE users;
```

Diretivas propostas: `up`, `down`, `transaction auto|always|never`, `dialect`, `statement-begin`, `statement-end`, `irreversible`, `requires`, `risk-acknowledgement` (a sintaxe final deverá ser congelada no formato v1).

- `up` obrigatório; `down` opcional, mas, se ausente, migration é declarada irreversível e `down` deve recusar explicitamente.
- Em pares, cada arquivo corresponde a uma direção; conteúdo duplicado/contraditório falha na validação.
- Blocos customizados delimitam statements com semicolons internos, stored procedures, triggers ou scripts complexos.
- Nunca fazer `str.split(';')`; scanner por dialeto respeita strings, comments, dollar quoting PostgreSQL, identificadores delimitados e blocos explicitamente anotados.
- Para MySQL/MariaDB, `DELIMITER` é comando do cliente CLI, não SQL do servidor; documentar sintaxe suportada para rotinas e rejeitar constructs não analisáveis sem anotação explícita.
- Para drivers SQL, executar instruções individualmente em canais com journal se não forem transacionais; não depender de `multiStatements` para garantir rastreamento.
- Substituição de variáveis SQL desativada por padrão (inclusive variáveis de ambiente). Metadados podem usar parâmetros de conexão, nunca interpolação de SQL com segredos.
- Mensagens de erro incluem caminho, número de linha/coluna e trecho sanitizado.

### 4.2 Tratamento de SQL opaco

Um parser de directives não é um verificador semântico completo de SQL. Classificar operações em:

- `recognized`: sintaxe compreendida pelo planner/linter, com riscos e capacidades conhecidos;
- `opaque_declared`: statement delimitado de forma confiável, mas efeito não inferido; permitido conforme política, com alerta;
- `unsupported`: limites de separação/semântica não cumpridos; recusar em vez de executar parcialmente.

`plan` indica garantias/limitações. `test` efetua execução em banco descartável e não equivale ao `plan`.

### 4.3 Motor de lint

Mínimo V1: mudança destrutiva, drop/truncate, alteração de tipo, nulidade de coluna, foreign keys possivelmente bloqueantes, índices sem estratégia de online DDL, transação inválida, SQL opaco, repeatable destrutiva, ausência de down e dependência de extensão não declarada.

Resultado inclui `rule_id`, `severity`, `dialect`, `location`, `explanation`, `recommended_action`, `confidence` e `can_auto_fix`; exceções explícitas versionadas com motivo, sem bypass automático por agente.

---

## 5. DSL tipada Rust e migrations Rust customizadas

### 5.1 Experiência da DSL (API ilustrativa, não assinatura final)

```rust
let mut schema = SchemaPlan::new();
schema.create_table("users", |t| {
    t.uuid("id").primary_key();
    t.varchar("email", 255).not_null().unique();
    t.timestamp_tz("created_at").not_null().default_now();
    Ok(())
})?;
schema.create_index("users_email_idx").on("users", ["email"])?;
let plan = schema.finish()?;
```

- DSL produz AST tipada, não SQL por concatenação textual.
- Identificadores validados e escapados por adaptador; valores parametrizados quando cabível; sem aceitar identificadores vindos do LLM sem validação.
- Tipos: integer/bigint, varchar/text, boolean, decimal, timestamp/timezone, binary, JSON, UUID, primary/unique/check, foreign keys, indexes, defaults.
- `uuid`, timezone e `json` variam entre dialetos; exigir estratégia explícita, não fingir equivalência semântica.
- `alter_table` usa capabilities e as regras reais do servidor; SQLite pode exigir rebuild seguro de tabela, que não será gerado automaticamente na V1 sem prova/teste.
- `raw_sql(sql, dialect, safety_class)` será escape hatch marcado como operação opaca com tipo de retorno explícito.
- Up e Down são registrados separadamente; não gerar down automaticamente para operações destrutivas. Pode sugerir um down, mas revisão é obrigatória.

### 5.2 Trait para migration programática

Desenho conceitual:

```rust
trait Migration: Send + Sync {
    fn identity(&self) -> MigrationIdentity;
    fn metadata(&self) -> MigrationMetadata;
    fn plan(&self, ctx: &PlanContext) -> Result<MigrationPlan>;
    fn up<'a>(&'a self, ctx: &'a mut MigrationContext)
        -> BoxFuture<'a, Result<()>>;
    fn down<'a>(&'a self, ctx: &'a mut MigrationContext)
        -> BoxFuture<'a, Result<()>>;
}
```

Assinaturas acima são orientativas; devem ser ajustadas para object safety, transações, async e ownership. Distinguir DSL estática (planejável) de Rust imperativo, para o qual `plan()` pode expor apenas precondições e operações explicitamente declaradas.

`MigrationContext` encapsula a sessão pinned e journal; operações SQL em Rust devem passar por esse contexto. O núcleo não impede que código Rust confiável acesse rede/filesystem por sua conta, logo **não prometer sandbox de Rust**. Código imperativo sem step tracking suficiente não pode declarar retomada segura após falhas não transacionais.

### 5.3 Runner compilado: contrato crucial

- O usuário cria e compila um runner Rust que registra SQL (filesystem/embedded) e migrations Rust/DSL.
- Se houver migrations Rust, **toda execução mista será delegada ao runner**. O processo runner carrega o plano completo, obtém o lock e executa uma única sequência ordenada. Nunca aplicar SQL pela CLI e Rust por um subprocesso separado no mesmo batch.
- CLI standalone pode executar projetos SQL-only diretamente, mas, ao detectar Rust pendente, exige runner registrado e compatível.
- Em modo embedded, a própria aplicação invoca a engine, explicitamente. A aplicação escolhe quando executar, jamais migração automática implícita ao inicializar biblioteca.
- Handshake CLI ↔ runner versionado: `protocol_version`, `engine_version`, `metadata_schema_version`, capabilities, fingerprint do manifest, plano hash e identidade do destino.
- Não enviar senha ou tokens nos argumentos de processo; usar variáveis seguras/pipe protegido ou resolução configurada no processo filho; logs com redaction.
- Runner rejeita plano cujo manifest/build não corresponde ao esperado; a CLI não faz fallback para execução parcial de SQL se runner falhar.
- O runner não compila código Rust; a compilação é comando explícito de build Cargo do projeto.

### 5.4 Fontes embutidas

API semelhante a `include_migrations!(...)`/`MigrationRegistry`, combinando arquivos versionados com módulos Rust registrados em compilação. Paths embedded são virtuais e preservados no histórico. O projeto deve testar corretamente identificação e checksum quando os arquivos fonte não existem no filesystem em produção.

---
## 6. Metadados internos e tabelas de controle

### 6.1 Conjunto de tabelas

**Quatro tabelas operacionais + uma tabela de versionamento interno** (portanto cinco objetos persistentes por padrão):

1. `schema_migrations`: estado corrente de versionadas, repeatables e migrations legadas importadas;
2. `schema_migration_runs`: trilha imutável de tentativas e transições de aplicação/rollback/importação;
3. `schema_migration_steps`: journal persistente de instruções potencialmente não transacionais;
4. `schema_migration_locks`: informações diagnósticas do proprietário/epoch da execução, sem substituir o lock nativo;
5. `schema_migration_meta`: versão do formato e identidade da instalação (bootstrap e upgrades internos).

Cada driver produz DDL próprio, a partir do mesmo contrato lógico. Nome/schema/tabelas podem ser configurados **antes** do bootstrap; alterações posteriores precisam de comando controlado de upgrade/relocação, não flags informais.

### 6.2 Identidade interna: evitar índices com nomes longos

Definir `migration_key` como digest hex SHA-256 de um descriptor canônico versionado; por exemplo, para nativos, `{key_format:v1,namespace,kind:native,version}`, e para legados `{key_format:v1,namespace,kind:legacy,origin_tool,origin_database_identity,origin_record_identity}`. Não usar o nome completo como chave primária: um `LONGTEXT` não é PK natural compatível com MySQL.

**Chave composta:** `(namespace, migration_key)`, em que `namespace` é identificador validado de tamanho limitado (sugerido `VARCHAR(128)`) e `migration_key` é `CHAR(64)` em ASCII/colação binária. As representações originais e completas ficam em `TEXT/LONGTEXT`, sem truncamento artificial. O hash é identificador técnico, **não** substitui nome, origem ou versão em relatórios. Validação verifica que o descriptor do registro encontrado corresponde ao descriptor recalculado, para evitar confundir identidades por bugs de canonicalização.

Para migrations nativas, `native_version` é uma coluna textual própria (`VARCHAR(20)`), com índice único parcial ou equivalente por `(namespace, kind, native_version)` quando houver valor. Para legadas, `original_version` é texto integral, sem conversão para `BIGINT` e sem obrigação de formato temporal.

### 6.3 `schema_migrations` — colunas lógicas

| Coluna | Tipo lógico | Regra |
|---|---|---|
| `namespace` | string 1..128 | obrigatório, parte da PK |
| `migration_key` | ascii(64) | obrigatório, parte da PK |
| `kind` | enum | `versioned`, `repeatable`, `legacy` |
| `native_version` | ascii(20)? | somente quando nativa timestamped |
| `original_version` | text? | preservar versão original do import |
| `filename` | text? | nome integral original, inclusive extensão; NULL quando fonte não sabe |
| `source_path` | text? | caminho lógico integral (sem credenciais) |
| `description` | text? | descrição opcional |
| `authoring_type` | enum | `sql`, `rust`, `dsl`, `legacy_unknown` |
| `origin_tool` | text? | `knex`, `goose`, `sqlx` ou custom |
| `origin_identity` | text? | identidade original, sem perda |
| `batch_id` | text? | grupo de aplicação/importação quando conhecido |
| `apply_order` | int64 | ordem real de aplicação na instalação |
| `content_checksum` | string? | digest + algoritmo/versionamento |
| `up_checksum` | string? | digest da seção Up quando disponível |
| `down_checksum` | string? | digest da seção Down quando disponível |
| `artifact_fingerprint` | string? | versão de binário/registry Rust, se verificável |
| `verification_status` | enum | `verified`, `partial`, `unverifiable` |
| `applied_at` | timestamp UTC? | não inventar datas ausentes de legado |
| `observed_at` | timestamp UTC | hora em que o registro foi lido/importado |
| `execution_ms` | int64? | duração, quando conhecida |
| `engine_version` | text? | versão do executor |
| `metadata_format_version` | int | formato do contrato |

`kind=repeatable` utiliza linha de estado representando **última versão aplicada**; runs guardam todas as reaplicações. Migration legada importada não terá obrigatoriamente checksum, filename ou `applied_at`; nulos devem ser visíveis, não convertidos em `0`, string fictícia ou data de importação. `applied_at` é diferente de `observed_at`.

### 6.4 `schema_migration_runs` — auditoria de execução

Mínimo: `run_id` UUID textual, `batch_id`, `namespace`, `migration_key`, `attempt`, `direction` (`up|down|repeat|import|reconcile`), `state` (`planned|started|succeeded|failed|indeterminate|reconciled`), `started_at`, `ended_at`, `duration_ms`, `requested_by` opcional, `actor_kind` (`human|ci|embedded|agent_proposal`), `target_fingerprint`, `plan_hash`, `checksum_at_execution`, `driver_version`, `server_version`, `runner_fingerprint`, `error_code`, `error_summary_redacted`, `reconciliation_evidence_ref`.

- Inserir tentativa antes de execução sempre que o driver permitir com segurança.
- Nos bancos transacionais, mudança + linha de estado (`schema_migrations`) devem ser atômicas dentro da mesma transação. Finalização do log de runs pode acontecer depois; caso caia, `doctor` distinguirá sucesso demonstrável de resultado indeterminado.
- Na atualização de estado por rollback, **remover linha de `schema_migrations` apenas no commit seguro do Down**, guardando todos os eventos em `runs`.
- Auditoria é append-only em termos semânticos: updates de lifecycle são permitidos, mas transições posteriores, reparos e intervenções humanas são eventos adicionais rastreáveis; nunca apagar evidências de falha em operações normais.

### 6.5 `schema_migration_steps` — journal por instrução

Mínimo: `run_id`, `step_index`, `step_key`, `direction`, `statement_hash`, `statement_kind`, `transaction_class`, `state` (`intent_recorded|acknowledged|failed|indeterminate|reconciled`), `started_at`, `acknowledged_at`, `error_code`, `evidence`, `attempt_id`.

- PK sugerida `(run_id, step_index)`; `step_key` estável é calculado a partir do plano e ordem.
- **Antes** de enviar statement não transacional, confirmar persistência de `intent_recorded`.
- **Depois** de confirmação explícita do servidor, marcar `acknowledged`.
- Timeout, cancelamento, morte de processo ou perda de conexão entre envio e ack => `indeterminate`. Não prosseguir nem executar próximo statement.
- Não gravar SQL bruto ou binds sensíveis por padrão: hash, identificação de objeto/categoria e localização de arquivo bastam; dump verbatim somente em diagnóstico autorizado, com saneamento.
- Instruções Rust com efeitos não rastreáveis terão visibilidade parcial, classificada como opaca; não declarar capacidade de recuperação não demonstrada.
- Journal não torna DDL do MySQL/MariaDB transacional nem garante atomicidade de operações externas ao banco.

### 6.6 `schema_migration_locks` — diagnóstico, não mutex suficiente

Mínimo: `scope`, `owner_id`, `session_id_redacted`, `lock_strategy`, `acquired_at`, `last_observed_at`, `run_id`, `lock_epoch`, `state` (`observed_active|released|lost|unknown`). TTL/heartbeat é diagnóstico; **não é autorização para roubar lock**. Novas execuções consultam lock nativo + estado de recuperação.

Estratégia conservadora V1: **um lock por banco/instalação de metadados**, mesmo com múltiplos namespaces, para evitar suposições de independência de schema. Locks por namespace em paralelo somente V2 ou adaptador que prove isolamento e matriz de recursos sem conflito. Isso não impede namespaces diferentes, apenas serializa alterações.

### 6.7 `schema_migration_meta` — versionamento do próprio migrator

Campos: `metadata_format_version`, `installation_id`, `initialized_at`, `upgraded_at`, `engine_min_version`, `dialect`, `ddl_fingerprint`, `features_enabled`.

- Bootstrap protegendo DDL dos metadados com lock nativo **antes de criar tabelas**.
- Bootstrap idempotente por inspeção do catálogo; se cair na metade, recusar alterações normais até `doctor metadata` reconciliar. A versão final só é confirmada após criação completa.
- `dbm metadata upgrade` tem plano e confirmação; `status`, `plan`, `validate` e `doctor --read-only` não fazem auto-upgrade.
- Backups/documentação obrigatórios para upgrades incompatíveis; engine antiga falha informando a versão necessária, sem adulterar dados.

### 6.8 Mapeamento físico por dialeto

| Conceito | PostgreSQL | SQLite | MySQL/MariaDB |
|---|---|---|---|
| PK `migration_key` | `CHAR(64)` | `TEXT` com constraint de tamanho | `CHAR(64) CHARACTER SET ascii COLLATE ascii_bin` |
| nome completo | `TEXT` | `TEXT` | `LONGTEXT` |
| timestamp de aplicação | `TIMESTAMPTZ` | texto ISO UTC ou microssegundos com codec estável | `DATETIME(6)` armazenado em UTC |
| inteiro de ordenação | `BIGINT` | `INTEGER` | `BIGINT` |
| enum lógico | `TEXT` + `CHECK` | `TEXT` + `CHECK` | `VARCHAR` + validação (CHECK conforme versão) |
| metadata DDL | schema qualificado | tabela no arquivo | esquema/banco qualificado |

Em MySQL, nomes de arquivos `LONGTEXT` **não são usados como PK/índice único**; o digest fixo mantém índices portáveis. Índices e constraints devem ser testados também em `utf8mb4` e collation adversarial. A API lógica deve impedir `namespace` inválido e usar identificadores SQL escapados por driver (parâmetros não substituem identificadores).

### 6.9 Consultas e invariantes

- `status`: comparar identidade, estado, checksum, nome, fonte, manifest, namespace e ordem com o catálogo local.
- `history`: recuperar linha de estado e todos os `runs`; deve ser possível filtrar inclusive migrations revertidas/importadas.
- `current_version`: máxima versão nativa **atualmente aplicada** por namespace, não máximo histórico.
- `doctor`: comparar `runs`, steps, estado atual, presença de lock nativo e objetos do catálogo para determinar estados provados/indeterminados.
- `batch_id` tem formato opaco (`UUID` ou ULID), separado de batch ordinal para compatibilidade com Knex; batch original do Knex é preservado em provenance.
- Importação e repeatables não podem reutilizar a mesma identidade do espaço nativo.

---

## 7. Executor e máquina de estados

### 7.1 Pipeline de execução

```text
source discovery
  → parser/registry
  → manifest + identity validation
  → capabilities/version checks
  → catalog + metadata read
  → validation + checksum/dialects
  → immutable migration plan
  → human/CI policy approval (mutating commands)
  → native lock acquisition
  → revalidate target / plan / source manifest under lock
  → run + per-step journal
  → execute with pinned connection
  → durable state transitions
  → release lock
  → result + audit + tracing
```

**TOCTOU:** é obrigatório recalcular a validação de estado sob lock antes do primeiro statement. `plan_hash` identifica plano e conteúdo de migrations; se arquivo, runner, versão do server, schema ou estado mudou entre `plan` e `up`, abortar ou exigir novo plano/aprovação.

### 7.2 Tipos de migration

| Tipo | Up | Down | Journal | Política |
|---|---|---|---|---|
| SQL transacional | Sim | Se definido | normalmente por run | atomicidade migration + estado |
| SQL não transacional | Sim | Se definido | por statement | fail-closed, reconciliation |
| Rust/DSL declarativo | Sim | Definido pelo autor | conforme SQL emitido | plano verificável |
| Rust imperativo | Sim | Opcional | steps declarados/registrados | efeitos opacos explicitados |
| Repeatable | Reaplica quando checksum muda | não automático | conforme driver | registrar toda execução |
| Legada importada | já aplicada; não reexecutar | bloqueado até adoção | evento de importação | preserve history |

### 7.3 Estados de execução

- `pending`: existe localmente e nunca foi aplicado;
- `planned`: reconhecido por um plano imutável;
- `running`: tentativa registrada, execução iniciada;
- `applied`: estado persistente confirmado;
- `failed`: falhou sem dúvida sobre resultado (ou transação inteira revertida);
- `needs_reconciliation`: efeito total/parcial desconhecido;
- `rolled_back`: evento de rollback confirmado; estado removido de `schema_migrations`;
- `imported`: estado herdado de outro migrator, sem execução nova;
- `diverged`: identidade/arquivo ou integridade não compatível com histórico;
- `blocked`: operação impedida por compatibilidade, policy, lock ou dependência.

`status` é calculado a partir de estado + runs + fontes, não um campo `status` mutável único capaz de conflitar com transações.

### 7.4 Transactions

- `auto`: utilizar transação somente se todas as operações são comprovadamente elegíveis; caso contrário a operação deve ser explicitamente marcada e revisada.
- `always`: rejeitar antes de executar quando houver statement/capability incompatível.
- `never`: executar com journal por step e política de risco reforçada.
- PostgreSQL e SQLite: aplicar mudança de estado na mesma transação do SQL quando possível.
- MySQL/MariaDB: DDL pode causar commit implícito, portanto não envolvê-lo numa transação que o executor anuncia como revertível. Etapas DML que exigem transação devem ser delimitadas explicitamente e verificadas.
- Migrações com mistura de DDL/DML terão planner capaz de indicar os limites reais de atomicidade.
- Execução de batch não é garantida atomicamente entre migrations; atomicidade por migration somente quando o banco suportar.

### 7.5 Migrations fora de ordem, batches e rollback

- Por padrão, migration nova com versão menor do que uma já aplicada é `out_of_order` e bloqueada.
- Opção específica para aplicar fora de ordem exige revisão de dependências e declaração de risco; sem reordenar registros antigos.
- `up`: todas as pendentes em ordem (ou `--to/--steps`); uma invocação gera `batch_id` único.
- `down`: última migration nativa aplicada, se Down disponível; não ultrapassa fronteira legada importada.
- `rollback --batch latest`: percorre em ordem inversa **da aplicação real** do batch, não ordenação lexical presumida.
- `down --to`: recusa atravessar dependências desconhecidas/repeatables afetadas e importados não adotados.
- `redo`: Down + Up são **duas ações auditadas** com approval, nunca mutação irreversível escondida; desabilitado em produção por padrão.
- `reset`: comando potencialmente destrutivo separado, não habilitado em produção por padrão.
- Sem Down: erro explícito `MIGRATION_IRREVERSIBLE`, sem tentar fabricar rollback.

---

## 8. Estratégias de lock e particularidades por banco

### 8.1 PostgreSQL (`tokio-postgres`)

- Conexão física pinned durante toda a execução.
- `pg_advisory_lock` de sessão com chave derivada de installation-id/escopo estável. Para locks específicos de banco, usar chave suficiente e documentar possíveis colisões de hash; metadados guardam identity.
- Lock é liberado explicitamente ou ao fim da sessão; transação COMMIT/ROLLBACK **não** libera advisory lock de sessão.
- SQL transacional por migration com estado gravado no mesmo COMMIT.
- Operações como `CREATE INDEX CONCURRENTLY` exigem fora de transação, classificação explícita e journal.
- Detectar perda da tarefa de I/O/conexão; não assumir que novo Client mantém mesmo lock.
- Conexão direta ou pool em modo de sessão; **rejeitar/alertar poolers em transaction pooling incompatível com locks de sessão**. Testar PgBouncer como negativo.
- Respeitar `lock_timeout` e `statement_timeout` configurados por sessão, com defaults conservadores e observabilidade; timeout é resultado possivelmente indeterminado se ocorrer perda de resposta.

### 8.2 SQLite (`rusqlite`)

- Executar operações num worker blocking dedicado à conexão, preservando ownership e ordem.
- `BEGIN IMMEDIATE` na própria conexão que executará statements transacionais e escreverá estado, garantindo exclusão de escritores ao longo da transação.
- `busy_timeout` configurável; avaliar comportamento com WAL/rollback journal e múltiplos processos.
- Para statements explicitamente não transacionais (`VACUUM`, certas PRAGMAs), a V1 **não afirma possuir lock transacional universal**: rejeitar por padrão ou exigir um protocolo especial de maintenance mode/file lock homologado em filesystem local. Não executar sobre NFS/SQLite remoto sob suposição de exclusividade sem teste.
- In-memory e databases attached terão escopo/restrições explícitos, evitando assumir exclusão multiprocesso inexistente.
- DDL e metadados na mesma transação quando suportado.

### 8.3 MySQL e MariaDB (`mysql_async`)

- Mesma base de cliente, porém detecção diferenciada de `version`, `version_comment` e capacidades de servidor.
- Conexão física pinned para lock e execução, jamais requisitando uma nova conexão do pool entre instruções que dependem da sessão.
- Lock com `GET_LOCK(name, timeout)` / `RELEASE_LOCK(name)`; `name` gerado de prefixo curto + digest determinístico, comprimento **até 64 caracteres** para compatibilidade MySQL.
- Lock de sessão não participa da transação; observar connection ID, deadlocks e perda de conexão.
- Atomic DDL no MySQL significa atomicidade **de uma instrução suportada**, não de múltiplos DDL nem de uma migration inteira.
- `schema_migration_steps` deve registrar intenção antes de qualquer statement não transacional e confirmação depois; um DDL concluído que perdeu ACK continua indeterminado.
- Só permitir `reconcile` após coleta de evidência explícita, aprovação humana e auditoria.
- Homologar MySQL e MariaDB **separadamente**: versões, collations, operações DDL, engines, indexes, comportamento de locks, protocolos TLS e falhas.
- Escopo oficial inicial sugerido para CI: MySQL 8.4 LTS; MariaDB 10.11 e 11.4/11.8 LTS. Acrescentar linhas estáveis atuais conforme compatibilidade comprovada, sem afirmar suporte apenas pelo fato de o driver conectar.

### 8.4 Contrato de capacidades do driver

Cada driver deve retornar estrutura `DatabaseCapabilities` com server/dialect/version, transações DDL, execução multi-statement segura, classes de índice, lock session/tx, scopes, schemas, rename/drop constraints, generated columns, tipos, introspecção, operações não transacionais, limits e support flags.

A engine compõe `requires` da migration com o que o driver comprova, resultando em `supported`, `unsupported` ou `unknown` (o terceiro falha fechado para operações críticas). Incompatibilidade retorna erro **antes** da aplicação.

### 8.5 Driver customizado

Contrato público mínimo, com interfaces separadas para:

```text
DatabaseAdapter
  connect(config) -> DatabaseSession
  detect_server() -> ServerInfo
  capabilities() -> CapabilitySet
  quote_identifier()
  metadata_ddl(version)
  introspect_catalog()
  lock_strategy()
  compile_schema_operation(AST) -> DialectPlan

DatabaseSession
  execute_statement()
  query_metadata()
  begin/commit/rollback()
  verify_session()
  close()

MigrationSource
  list()
  read(identity)
  source_fingerprint()
```

`Connection` e lock têm lifetimes explícitos; driver pode usar futures encaixotadas para object safety. Adapter fornece testes de conformidade reutilizáveis. Adapters terceiros distribuídos como crates + runner recompilado, não `.so` carregado dinamicamente.

---
## 9. Importação de Knex, Goose e SQLx — V1, com cutover seguro

### 9.1 Princípio

**Importar histórico não é executar migration**. O importador lê metadados legados, constrói identidade e proveniência, comprova o que for possível, registra um snapshot auditável e adota o estado corrente. A ferramenta antiga deve ser operacionalmente congelada antes do cutover; locks do migrator novo não impedem que um migrator antigo diferente escreva em paralelo.

### 9.2 Mapeamento por ferramenta

**Knex**:
- Ler tabela histórica `knex_migrations`: `id`, `name` (nome integral), `batch`, `migration_time`, considerando nomes/schema configurados; não presumir prefixo padrão.
- `knex_migrations_lock` mantém `is_locked` e `index` para o locking Knex; **não** duplica nomes de migrations. Inspecionar lock aberto e recusar cutover concorrente.
- Preservar nome `.js/.ts`, batch e ordem `id`; não executar JavaScript/TypeScript no Rust.
- Sem checksum histórico original, marcar como `unverifiable` salvo evidência externa; hash do arquivo atual é apenas evidência contemporânea, não prova de seu conteúdo no passado.

**Goose**:
- Ler `goose_db_version` (`id`, `version_id`, `is_applied`, `tstamp` no formato usual), incluindo eventos de Up/Down; calcular migrations **atualmente aplicadas** conforme histórico ordenado por `id`, preservando eventos de reversão.
- Versão 0 sentinela não é migration de aplicação.
- Histórico pode não ter filename, descrição nem checksum; guardar campos desconhecidos como NULL, não inferir a partir de arquivos por coincidência de versão.
- Preserve timeline original num import report separado e/ou na trilha `runs` importada, com marcador de confiabilidade.

**SQLx**:
- Ler `_sqlx_migrations`: versão, descrição, `installed_on`, `success`, `checksum`, `execution_time`, considerando variações de versão/driver.
- Preservar versão i64 como texto no modelo novo e checksum original SHA-384, sem converter para SHA-256 com a alegação de representar o histórico aplicado.
- Linhas `success=false` são bloqueadoras: diagnosticar e reconciliar antes do cutover, nunca tratá-las como aplicadas com sucesso.
- Migrações `.up.sql` e `.down.sql` e irreversíveis devem ser catalogadas por identidade legada, sem prometer rollback nativo até conversão.

### 9.3 Fluxo de importação

1. `import inspect --from knex|goose|sqlx` — somente leitura, conexão à base legado, descoberta de tabelas e versionamento.
2. Gerar relatório com origem, identidade do banco, rows, estado final, tipos, lacunas de integridade, incompatibilidades e arquivos faltantes.
3. `import plan` — cria snapshot canônico e plano de cutover imutável com digest; compara com migrations locais e detecta colisões e versões fora de ordem.
4. Pausar deploys/migrator antigo por procedimento documentado, efetuar backup e garantir ausência de execução ativa.
5. `import apply --approve-plan <digest>` — adquirir lock nativo da instalação nova, reler histórico original **no mesmo alvo** e confirmar que não mudou desde o plano.
6. Criar metadados novos de maneira protegida e registrar estado aplicado + provenance de importação, **sem executar SQL legado**; commit conforme garantia do banco.
7. `import verify` — comparar conjunto, ordem, hashes disponíveis, nomes e estado de origem com os persistidos.
8. Registrar cutover e configurar pipeline para executar **somente o migrator novo**. Não escrever nas tabelas Knex/Goose/SQLx nem apagá-las automaticamente.

Se existir `schema_migrations` inicializada, recusar importação duplicada até análise/reconciliação explícita. `--dry-run`/`plan` não cria tabelas, não atualiza estado e não escreve marca de migração.

### 9.4 Regra de rollback legado

- Toda identidade importada inicia bloqueada para rollback.
- `legacy adopt <identity>` exige migration convertida, matching de identidade, revisão humana, prova em banco descartável e vínculo ao item histórico.
- Só após registrar a conversão verificada o Down passa a ser permitido; nunca executar `.js` Knex, `.go` Goose nem Rust legado por suposição.
- Importações Knex/Goose/SQLx mantêm seus IDs originais, mesmo quando o nome do arquivo não está disponível.

### 9.5 Adotar banco existente sem histórico

`baseline` é operação separada de `import`. Precisa de snapshot do catálogo esperado, comparação real, análise das lacunas e aprovação explícita. Nunca marcar um diretório inteiro como executado por simples argumento `--baseline latest` sem verificar o banco. Diferenciar fonte de verdade (catálogo observado, migrations desejadas, histórico ausente).

---

## 10. Repeatables, namespaces e dependências

### 10.1 Repeatables

- Arquivos `R__name.sql` identificados por nome lógico estável, não versão temporal.
- Reexecutar após aplicar versionadas quando `content_checksum` ou contrato explícito de dependência mudar.
- Ordenação estável por diretório registrado + nome; nenhum reordenamento silencioso ao trocar OS/filesystem.
- SQL repeatable pode declarar `up` ou um corpo principal, mas não recebe rollback automático; uma view atualizada deve ser restaurada por novo conteúdo ou migration corretiva.
- Toda reexecução gera novo run; estado guarda última versão de conteúdo aplicada.
- Repetição potencialmente destrutiva exige lint/revisão e idealmente idempotência testada.

### 10.2 Namespaces e fontes

- Namespace default `default`, mas uma aplicação pode declarar `auth`, `billing`, `tenant_a`, etc.
- O namespace organiza identidade e filtragem; **não implica isolamento físico** de tabelas nem permissões SQL.
- Permitir várias fontes (pastas, recursos embedded e custom) com precedência e nomes lógicos documentados; colisão => erro.
- Seleção de alvo explícita (`--target`, `--namespace`). Nunca usar `--all-tenants` implícito; se existir futuramente, terá plano por alvo e aprovação multiprojeto.
- Versionadas de vários namespaces não são ordenadas por uma suposta dependência global a menos que isso seja declarado. V1 usa execução serial e restrições de dependências explícitas simples; dependências cross-namespace complexas podem exigir runner orquestrador controlado.

### 10.3 Catálogo, schema drift e limites

Separar claramente:

1. **File/history drift**: arquivo, nome, versão ou checksum divergem do que foi aplicado; cobertura obrigatória V1.
2. **Schema drift**: catálogo real difere de um snapshot/estado esperado; V1 oferece introspecção read-only e comparação estrutural básica (tabelas, colunas, índices e constraints cuja semântica o driver consiga canonizar). Objetos não suportados são `unknown`, não falso `equal`.
3. **Desired-state diff**: comparar schema declarativo desejado a schema real e sugerir mudanças; expandir em V2. Não gerar migrations destrutivas automaticamente.

### 10.4 Seeders e data migrations

- **Data migrations** que precisam participar da mesma ordem e do mesmo histórico de schema são migrations comuns SQL/Rust, declaradas com classe `data`, com journal e políticas de transação usuais.
- **Seeders** são operação distinta e opcional: fonte/registry próprios, esquema de execução próprio, e nunca entram em `schema_migrations` ou em rollback de schema por acidente.
- Contrato mínimo V1: API/CLI para executar seeds deliberadamente em alvos de desenvolvimento/teste, com ordem explícita, namespace e confirmação; default `deny` em produção. Seeders não são implicitamente idempotentes nem automaticamente revertíveis.
- Features avançadas de seed (faker, fixtures complexas, anonimização de dados, distribuição remota) ficam para V2, sem contaminar o core do migrator.

---

## 11. CLI, configuração, ergonomia e API agent-friendly

### 11.1 Comandos planejados

```bash
# Inicia estrutura e configuração
dbm init --driver postgres
dbm new create_users --format sql
dbm new create_users --format sql-pair
dbm new migrate_users --format rust
dbm new create_orders --format dsl

# Lê e valida, sem mutação
dbm status --target dev
dbm list --namespace default
dbm validate
dbm lint --format json
dbm plan --target staging --format json
dbm history --target staging
dbm schema inspect --target dev --format json
dbm schema diff --target dev --snapshot schema.json
dbm test --shadow-target isolated_test_db  # EXPLÍCITO: executa em banco descartável

# Execução explicitamente autorizada
dbm up --target dev
dbm up --to 20261021162643981234 --target staging
dbm down --steps 1 --target dev
dbm rollback --batch latest --target dev

# Diagnóstico e importação
dbm doctor --target staging
dbm doctor reconcile --run-id <id> --evidence <arquivo>
dbm import inspect --from knex --target migration-db
dbm import plan --from goose --target migration-db
dbm import apply --from sqlx --target migration-db --approve-plan <hash>
dbm import verify --from sqlx --target migration-db

# IA opcional
dbm ai generate "Adicionar sessions com expiração" --dialect postgres
dbm ai review migrations/versioned/20261021162643981234_create_users.sql
dbm ai explain --plan plan.json

# Metadados
dbm metadata status
dbm metadata upgrade --approve-plan <hash>

# Seeders segregados das migrations de schema
dbm seed run --target dev --source seeds
```

**Comandos e nomes são parte da especificação proposta, sujeitos a revisão de nomenclatura antes de congelar o CLI v1.** A semântica de segurança deve ficar estável mesmo que nomes mudem.

### 11.2 Configuração (`dbm.toml`, proposta)

```toml
version = 1
namespace = "default"
source = ["migrations/versioned", "migrations/repeatable"]

[migration]
version_format = "utc-microseconds"
sql_format = "single-up-down"
out_of_order = "deny"
checksum = "sha256-raw-v1"

[targets.dev]
dialect = "postgres"
url_env = "DATABASE_URL"
environment = "development"

[targets.production]
dialect = "postgres"
url_env = "PRODUCTION_DATABASE_URL"
environment = "production"
allow_automatic_apply = false

[execution]
lock_timeout_seconds = 30
statement_timeout_seconds = 300
allow_opaque_sql = false

[ai]
enabled = false
provider = "openai-compatible"
profile = "default"
```

Precedência: flags CLI > variáveis de ambiente explicitamente documentadas > target TOML > defaults seguros. `url_env` referencia **nome da variável**, nunca armazena secrets no TOML. Arquivos `.env` são conveniência local opt-in, não necessários no runtime. Regras de config: detectar campos desconhecidos críticos, interpolação SQL desativada, configuração de produção exige marcador explícito, namespaces validados.

### 11.3 JSON machine-readable estável

Exemplo:

```json
{
  "schema_version": "1.0",
  "command": "plan",
  "target": "production",
  "namespace": "default",
  "status": "requires_approval",
  "plan_hash": "sha256:...",
  "read_only": true,
  "operations": [
    {
      "migration_id": "20261021162643981234",
      "filename": "20261021162643981234_create_users.sql",
      "direction": "up",
      "transaction": "supported",
      "risk_level": "medium",
      "capability_status": "supported"
    }
  ],
  "warnings": [],
  "errors": []
}
```

- `stdout`: exatamente um JSON válido quando `--format json`; progresso/logs exclusivamente em stderr estruturado (ou silêncio configurado).
- Schema JSON versionado, campos aditivos por minor, mudanças incompatíveis por major.
- `--non-interactive`: falhar se aprovação necessária; nunca fazer fallback para `yes` implícito.
- Exit codes: `0` sucesso; `2` CLI/config inválida; `3` migration inválida; `4` divergência; `5` lock indisponível; `6` execução falhou com estado conhecido; `7` estado indeterminado; `8` importação/baseline; `9` aprovação/permissão; `10` conectividade/driver; `11` IA/LLM; `12` metadados incompatíveis; `13` capacidade/dialeto não suportado. Especificar e testar invariância pública.
- Tratamento de sinais: sinalizar cancelamento, tentar concluir transação ou determinar falha segura; `SIGKILL` não oferece cleanup e é coberto por recuperação.
- Suporte a shell completions, `--help`, `--version`, `NO_COLOR`, `--quiet`, terminal colorido somente se TTY.

### 11.4 Teste real em banco descartável

- `dbm test` **executa de verdade**, diferentemente de `dbm plan`. Requer shadow target explicitamente indicado e comprovadamente distinto dos targets de produção; se não puder verificar isolamento, recusar.
- O teste sobe uma baseline conhecida e executa `up`/`down` no ambiente descartável, com as mesmas capabilities e versões do banco pretendido na medida do possível; gera relatório com sucesso, tempos, bloqueios, diff do catálogo e objetos opacos não verificáveis.
- Credenciais de criação/remoção do banco de testes são próprias. Não usar a URL de produção para criar um schema temporário no mesmo servidor sem autorização explícita e regras adicionais.
- Driver customizado pode fornecer `ShadowEnvironmentProvider`. No modo standalone, usar target preprovisionado ou SQLite temporário; containers e infraestrutura de teste são opcionais e não pertencem ao runtime normal.

### 11.5 Segurança em produção

- O `environment=production` é configurado explicitamente, não inferido de domínio/hostname.
- Qualquer operação mutável em produção exige `plan_hash`, fingerprint do destino e aprovação explícita; em CI, aprovação recebida fora da execução do agente (artefato de aprovação / gate do pipeline).
- `--force` genérico não existe. Overrides de segurança devem ter escopo, motivo e registro.
- O `plan` pode se conectar ao banco para leitura; **não** cria metadados, lock de escrita ou qualquer side effect durável.
- Rollback, redo, reset, reconcile e legacy-adopt têm políticas de autorização distintas.
- Ao gerar SQL com IA, salvar patch e pedir revisão; comando `ai` não oferece `--apply` no fluxo de geração.

---

## 12. IA / BYOK / integração com agentes

### 12.1 Arquitetura opcional

```text
User or coding agent
      ↓
CLI request + schema context (opt-in)
      ↓
AIProvider (OpenAI-compatible HTTP abstraction)
      ↓
Structured proposal (files + rationale + risk)
      ↓
Deterministic validator / linter / dialect capabilities
      ↓
Optional shadow DB execution and rollback tests
      ↓
Diff to be reviewed (no automatic production apply)
```

- BYOK obrigatório na V1: OpenAI-compatible endpoint customizável, incluindo opções como OpenRouter/Groq e provedores locais compatíveis, sem forçar uma empresa.
- Configuração de múltiplos profiles com URL, modelo, limites de tokens/custo, timeout, retry apenas para geração read-only e secret via env/secret store.
- Não embutir credencial em logs, telemetry, CLI history, plano serializado ou payload de auditoria.
- `ai generate`, `ai review`, `ai explain`, `ai suggest-down` (somente proposta).
- Operação com IA completamente opcional e compilável fora do binário minimal.

### 12.2 Contexto enviado ao modelo

- Por padrão: dialeto e versão relevante, capabilities, schema introspection mínima, nomes de tabelas/colunas necessários, migrations locais explicitamente selecionadas e pedido do usuário.
- Nunca enviar linhas reais, dados pessoais, valores de colunas, connection URL, tokens, segredos, dumps completos ou métricas sensíveis por padrão.
- Preview de contexto/payload antes de enviar quando `--preview-context`; suporte a allowlist de objetos, redaction e bloqueio de contextos proibidos.
- Rejeitar respostas que tentam invocar tools para aplicar migration, exfiltrar dados ou desabilitar validação; texto de SQL e schema é **dado não confiável**, não instrução hierarquicamente superior.

### 12.3 Artefato de geração

A resposta do LLM não é SQL executado diretamente. A engine valida um `MigrationProposal` estruturado:

```text
MigrationProposal:
  proposal_id
  requested_change
  dialect + capabilities_snapshot
  files[] (path, content, type)
  expected_schema_changes[]
  suggested_down[]
  uncertainties[]
  risk_explanations[]
  provider_metadata_redacted
```

Pipeline: validar schema do output → parse de arquivos → lint → checar nome/identidade → compiler dialect → gerar plano read-only → executar teste descartável se autorizado e disponível → apresentar diff → revisão humana. IA não pode auto-aprovar warnings, alterar história ou inventar evidências de tests.

### 12.4 Agent UX

- JSON Schema público para `MigrationProposal`, `Plan`, `Review`, `DoctorReport` e `Error`.
- Instrução oficial/Agent Skill para agentes de código: inspecionar → gerar arquivo → validar → testar em shadow DB → solicitar aprovação → nunca aplicar produção sem gate.
- Endpoint/servidor MCP local via stdio fica planejado para V2; V1 utiliza CLI/JSON e não depende de agentes específicos.
- Testar respostas malformadas, provider timeout, rate-limit, modelo alucinando coluna inexistente, prompt injection via SQL comments e tentativas de editar migrations aplicadas.

---

## 13. Doctor, diagnóstico e reconciliação

### 13.1 Comandos read-only

`dbm doctor --target NAME` verifica:

- versão/DDL do metadata schema e integridade das tabelas;
- estado de migrations vs source files e manifest;
- tentativas `started` sem resultado final;
- steps `intent_recorded` sem acknowledgement;
- estado do lock nativo e eventuais dados diagnósticos obsoletos;
- catálogo observado em relação a passos reconhecíveis;
- integridade de importações e itens legados irreversíveis;
- divergência entre checksum/nomes, dialetos e versões;
- execuções reentrantes e erros de conexão/session reuse.

Retornar diagnóstico `known_applied`, `known_not_applied`, `indeterminate`, `metadata_inconsistent`, `source_diverged` ou `lock_held`, com evidências verificáveis.

### 13.2 Reconciliação manual

- Comando `doctor reconcile --run-id` exige plano de reparo, evidência estruturada ou anotação de verificação externa, motivo, ator e aprovação.
- Exibir quais steps estão confirmados, quais não podem ser demonstrados e como cada hipótese pode ser inspecionada.
- Não autorizar simples `mark-success`/`mark-failed` que esconda execução indeterminada sem declaração explícita do operador e registro auditável.
- Nunca rodar automaticamente novamente DDL após um crash por falta de linha em `schema_migrations`.
- Em caso de perda de resposta do COMMIT transacional, verificar estado de `schema_migrations` sob nova conexão pode determinar resultado; guardar a conclusão como evento de reconciliação, não descartar evidência da dúvida inicial.
- Condição para voltar a executar: estado consistente confirmado, lock nativo disponível, plano recalculado e autorização pertinente.

### 13.3 Teste de cenários de falha obrigatório

1. Morte antes de registrar intent.
2. Morte após intent, antes de enviar SQL.
3. Morte durante SQL (sem resposta do banco).
4. SQL concluiu e processo morreu antes do acknowledgement.
5. Acknowledgement persistido e processo morre antes de atualizar estado final.
6. DDL parcialmente executado em MySQL/MariaDB e terceiro step falha.
7. Perda da sessão que possuía lock nativo; outra instância tenta iniciar.
8. Duas CLIs disputando o mesmo DB e namespace.
9. Dois namespaces compartilhando objeto de schema.
10. Falha no meio de bootstrap/upgrade das tabelas internas.
11. Rollback seguido de crash antes de registrar run.
12. Plan e source alterados entre aprovação e execução.

Critério: em nenhum cenário uma segunda instância pode *presumir* que statement desconhecido não executou; toda continuação deve ser bloqueada ou fundamentada por evidência verificável.

---
## 14. Segurança, auditoria, privacidade e supply chain

### 14.1 Modelo de ameaças

Ameaças obrigatórias: SQL malicioso em arquivo local ou gerado por IA; prompt injection embutido em comentários SQL; vazamento de credenciais via logs e argumentos CLI; alteração de arquivos após aprovação; migration destrutiva; corrida entre processos; perda de lock; indeterminação após timeout; truncamento de identificadores; importação de histórico fraudulento; driver customizado defeituoso; downgrade do metadata schema; execução de runner não confiável.

Mitigações:

- Emissão de `target_fingerprint`, `plan_hash`, checksum da fonte e versão do runner para todas as mutações.
- Runner/Rust custom é código confiável com permissões do processo, **não sandbox**; documentar a fronteira e usar revisão de dependências.
- Defaults fail-closed para impacto alto, operações opacas e mismatch de identidade.
- Senhas em env/secret manager, nunca em flags/URLs impressas; redaction de URLs, dados vinculados, `Authorization`, tokens AI e payloads privados.
- Validação de paths/symlinks e proteção contra traversal nos geradores e sources; arquivos editados entre `plan` e `apply` provocam novo hash e abortam.
- TLS configurável e validação de certificado ligada por padrão para bancos remotos e LLMs; insecure mode explícito, fora de produção.
- Privilégio mínimo por alvo, com credenciais de migração separadas das credenciais de runtime da aplicação quando possível.
- Backups/snapshots e janela operacional para mudanças destrutivas; linter não garante zero downtime.
- Toda aprovação é ligada a usuário/CI, alvo e hash do plano; nunca baseada apenas no texto `yes`.
- Publicação com checksums SHA-256 dos artefatos, SBOM, release notes, changelog e assinatura de releases onde viável.

### 14.2 Dados pessoais e observabilidade

- Não consultar amostras de dados de usuário para IA por padrão; schema metadata também pode ser sensível, então envio é opt-in.
- Logs por padrão não armazenam SQL bruto nem bind values. Expor opt-in local com confirmação e sanitização.
- Traces `migration.started|completed|failed`, com tags de baixa cardinalidade; não expor `filename`, `migration_id` ou `run_id` como label de métricas Prometheus.
- Métricas por driver/operação/resultado: execução, espera pelo lock, falhas, reconciliações, imports, validação e latency histogram. Export via hooks/tracing, sem abrir porta HTTP no processo CLI.
- Rotação e retenção do audit log serão gerenciadas pelo operador; API de exportação preserva evidência sem excluir linhas ativas por padrão.

---

## 15. Testes, ferramentas de qualidade e compatibilidade

### 15.1 Pirâmide de testes

**Unitários:** IDs/timestamps, colisões, canonicalização, hashes, ordenação/batches, AST, compile SQL por dialeto, scanner de quotes/procedures, error mapping, JSON schema e config.

**Property-based/fuzz:** sequências arbitrárias Up/Down, parser/delimitadores, Unicode/UTF-8, nomes longos, paths, timezones, timestamps concorrentes, schema diff canonicalizer e equivalência de fingerprints.

**Integração real:** migrator completo + banco real, com metadados v1, locks, transações, repeatables, source embedded, custom driver de teste, importação e rollback.

**Conformidade de driver:** conjunto compartilhado com versões/capabilities, DDL, locks, network errors, introspecção, formatos de timestamp, charset/collation, SQL identifier quoting e operações não suportadas.

**End-to-end da CLI:** binário standalone, runner compilado, aplicação embedded; conferir `stdout`, `stderr`, exit codes, `--format json`, `--non-interactive` e ausência de side effect em `plan`.

**Fault injection:** matar processo, desconectar banco, bloquear locks, induzir timeout, modificar migrations entre plan/apply e reiniciar o migrator.

**IA:** mocks determinísticos, integração opt-in com provider real via secrets em CI controlado, geração malformada, prompt injection, no-execution, limites de custo e redaction.

**Legado:** fixtures criadas por versões reais e fixadas do Knex, Goose e SQLx. Scripts para produzir fixtures separados dos testes normais, sem exigir Node/Go/SQLx ao usar a ferramenta em produção.

### 15.2 Matriz de homologação V1

| Categoria | Matriz mínima proposta | Particularidades |
|---|---|---|
| SQLite | bundled e system SQLite documentados | WAL, busy timeout, file locking, in-memory |
| PostgreSQL | 16, 17, 18 e versão estável adicional homologada | transações, advisory locks, DDL fora de transação, poolers |
| MySQL | 8.4 LTS como base; versão adicional atual em CI exploratória | GET_LOCK, 64-char, InnoDB, implicit commit |
| MariaDB | 10.11, 11.4 e 11.8 LTS | locks, tipos, DDL, collation, server features |
| Linux | x86_64 e aarch64 | musl/glibc conforme build validado |
| macOS | arm64 e x86_64 | SQLite system/bundled e notarização **não** é requisito de CLI |
| Windows | x86_64, condicionado a testes E2E verdes | paths, line endings, SQLite locking, TLS |

Versões adicionais podem entrar na matriz após validação real. O suporte deve ser publicado como matriz explicitamente homologada, não como `MySQL-compatible = todos os servidores`. Fornecer contêineres de teste via Docker/Podman, e aceitar orquestradores alternativos que implementem as mesmas fixtures; nenhuma dependência de contêiner é necessária no produto final.

### 15.3 Cenários de aceitação cross-dialect

- Executar plano de 500 migrations sequenciais e reiniciar em pontos aleatórios.
- Iniciar 10 processos simultâneos tentando migrar o mesmo alvo; no máximo um executa mutações em cada intervalo de lock, os demais aguardam ou falham de forma previsível.
- Aplicar SQL contendo quoted semicolon, dollar quoting, triggers e routines; rejeitar instruções não reconhecidas sem anotação.
- Rodar rollback de último batch e confirmar estado do DB, do histórico e idempotência do status.
- Importar histórico com filename ausente, checksum ausente, migração revertida e migration malsucedida.
- Executar SQL-only sem runner; misturar Rust/SQL e confirmar delegação do plano inteiro ao runner compilado.
- Executar `plan` sobre banco sem metadados e provar ausência de alterações DDL e de arquivos alterados.
- Forçar desconexão antes/depois do ACK e confirmar classificação correta de execução conhecida vs indeterminada.
- Modificar arquivo aplicado (inclusive seção Down) e obter divergência sem opção silenciosa de ignorar.
- Reaplicar repeatable alterada e confirmar que só sua identidade corrente muda, preservando runs anteriores.
- Inserir filename >255 caracteres com collation Unicode e testar preservação ou erro de filesystem antecipado sem truncamento.

### 15.4 Quality gates do repositório

- `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, testes unitários e docs de API.
- `cargo deny`/`cargo audit`, revisão de licensing, supply chain e dependências com advisories.
- Testes de integração por driver em PR (subconjunto rápido) e matriz completa noturna/pré-release.
- Suites de importação real e fault injection como gate de release.
- Sem benchmarks falsificados: publicar ambiente, método, warmup, nº de amostras e regressão.
- Coverage quantitativa por módulo, mas nenhum percentual substitui testes das invariantes de segurança. Prioridade para engine, metadata, locking, importação e recovery.
- Regressões reportadas como testes antes da correção, e todo bug de corrupção/inconsistência gera novo fixture permanente.
- Examples devem compilar e executar na CI, não apenas aparecer como pseudocódigo na documentação pública.

---

## 16. Performance, resiliência e metas não funcionais

### 16.1 Objetivos

- Custo marginal do migrator deve ser pequeno perante latência do DDL, evitando abrir conexão por statement.
- `status` e `plan` devem ser previsíveis e fazer poucas consultas (carregar histórico em lote, sem N+1).
- Executor somente uma migration mutável por banco/instalação de metadados na V1; não tentar otimizar concorrência à custa de segurança.
- SQL parser e DSL compilação devem funcionar offline; IA nunca é exigência para usar `validate`, `plan` ou `up`.
- Cancelamento gracioso não altera a semântica do journal; perda abrupta pode exigir `doctor`.
- Queda do provedor de IA ou indisponibilidade da Internet não afeta o migrator core.

### 16.2 Benchmarks e SLOs de engenharia propostos

- Suite offline com 1.000 migrations: descoberta + parse + plano dentro de 1 s no hardware de referência da CI, como objetivo inicial ajustável mediante baseline documentado.
- Benchmark por driver: tempo de `status`, `plan`, acquisição/release do lock, overhead por statement, bootstrap, migrations repeatables e leitura de histórico com 10 mil registros.
- Comparar o desempenho com Goose e SQLx CLI em conjunto de testes equivalente, diferenciando tipo de conexão, engine, número de migrations e política de transação.
- Meta de regressão: variação >20% em benchmark estável exige investigação antes do release, não reprovação cega de CI ruidoso.
- Tamanho e cold-start de CLI minimal/full medidos por alvo e publicados; sem prometer binário específico em MB sem medição.

**Baseline local inicial (2026-10-26; não é SLO nem comparação entre máquinas):**
`cargo build --release --bin dbm --locked` e
`sh scripts/bench-validate.sh target/release/dbm 1000 5`, com 1 warm-up e 5
medições de parede para `validate` offline sobre 1.000 migrations SQL geradas:
12, 11, 10, 10 e 11 ms (mediana 11 ms). Hardware/software: Intel Core
i5-8265U, Linux x86_64 kernel 7.2.8, `rustc`/Cargo 1.99.1, perfil release.
O harness confirmou que o relatório contou as 1.000 migrations e que nenhuma
base SQLite foi criada. O valor inclui startup do processo; mede apenas
descoberta/validação, não `plan`, drivers de rede, estado de metadata, concorrência
nem comparação com Goose/SQLx. Não serve como prova do objetivo de 1 s em hardware
de CI; repetir em hardware de referência e ampliar a matriz antes de usar como
critério de release. O script usa `/tmp/opencode` por padrão; `DBM_BENCH_TMPDIR`
pode selecionar outro diretório descartável.

### 16.3 Resiliência

- Erros de rede antes de qualquer statement podem permitir retry apenas da **conexão** em operações read-only; nunca fazer retry cego do DDL depois de envio.
- `retry` em lock acquisition pode aguardar com timeout e cancelamento; nenhum heartbeat permite usurpação de lock ativo.
- Planos aprovados expiram em mudança do banco/schema/source, não apenas por cronômetro.
- Timeouts distinguem falha da chamada cliente de prova de não execução no servidor.

---

## 17. Roadmap em fases com entregáveis e gates

As fases abaixo são **incrementos internos**. Uma fase intermediária não é anunciada como V1 pública completa. A V1 só recebe tag estável após todos os gates G0–G12.

| Fase | Entrega | Dependências | Gate de aceite |
|---|---|---|---|
| **F0 — Fundação** | ADRs, workspace, política de versão, CI, threat model, fixtures | nenhuma | G0: builds reproduzíveis, crate boundaries e decisões documentadas |
| **F1 — Identidade/fonte** | timestamps, nomes, hash, source filesystem/embedded, parser de manifest | F0 | G1: identidade estável, colisões/renomeação detectadas |
| **F2 — SQL básico/SQLite** | parser SQL, planner, SQLite executor, meta DDL inicial, Up/Down | F1 | G2: SQL-only SQLite end-to-end e rollback |
| **F3 — State/journal/recovery** | runs, steps, FSM, doctor read-only, reconciliation, locks SQLite | F2 | G3: fault injection não reaplica statement indeterminado |
| **F4 — PostgreSQL** | driver, session lock, transações, no-transaction, introspecção | F3 | G4: suíte PG 16/17/18 e concorrência aprovadas |
| **F5 — MySQL/MariaDB** | mysql_async, dialect capabilities, journal DDL, locks | F3 | G5 aprovado em 2026-10-24: matriz MySQL/MariaDB e crash recovery |
| **F6 — DSL Rust** | AST, compiladores por dialeto, escape hatch, migrations declarativas | F4/F5 | G6 aprovado em 2026-10-24: operações básicas nas quatro famílias |
| **F7 — Runner/embedded** | registry, protocolo CLI-runner, source embedded, Rust custom | F6 | G7: plano misto inteiro executado num runner; sem interpretador |
| **F8 — Importação legada** | Knex/Goose/SQLx inspect/plan/import/verify/adopt | F3/F4/F5 | G8: fixtures reais, provenance, rollback bloqueado |
| **F9 — Produto CLI/Repeatables** | comandos completos, config, JSON, batches, repeatables, namespaces | F7/F8 | G9: contratos CLI/JSON estáveis, CI não interativa |
| **F10 — IA opcional** | BYOK, generate/review/explain, schemas, contexto seguro | F9 | G10: nenhum caminho AI→auto-apply; geração validável |
| **F11 — Hardening** | security review, fuzz, chaos, performance, docs, exemplos, supply chain | todas | G11: matriz inteira verde, sem issues críticas abertas |
| **F12 — V1 estável** | releases multiplataforma, changelog, migração de metadata, suporte | F11 | G12: checklist final completo e rollback/doctor demonstrados |

**Status de execução (2026-10-26):** F4/G4 foi aprovado após a matriz PostgreSQL
16/17/18, TLS, PgBouncer adversarial e jornada CLI. F5/G5 foi aprovado após a
matriz MySQL 8.4 e MariaDB 10.11/11.4/11.8, com falha, deadlock, lock e
recuperação explícita. F6/G6 e F7/G7 foram aprovados após compilação/execução da
DSL e do runner. F8/G8, F9/G9 e F10/G10 foram verificados localmente conforme a
evidência abaixo. F11 está parcial, G11 não foi aprovado e F12/G12 continuam
bloqueados. Nenhum desses
marcos intermediários conclui ou autoriza anunciar a V1.

**Estimativa de planejamento, não promessa de prazo:** a V1 tem escopo equivalente a vários projetos médios. Para um engenheiro Rust experiente em dedicação integral, reservar aproximadamente **24–36+ semanas** como faixa de planejamento, com maior incerteza em parsing por dialeto, compatibilidade MySQL/MariaDB, importação legada e testes de falhas. Mais contribuintes não reduzem linearmente o tempo de integração. Fazer PoCs verticais de F2/F3/F5 nas primeiras semanas para calibrar esta estimativa.

### 17.1 Gates detalhados por fase

**G0** — Crates compõem sem ciclos; decisões sobre MSRV, API de extensões, metadados e invariantes no `docs/adr`; CI em Linux/macOS com formatação, Clippy e testes mínimos; threat model aprovado.

**G1** — 20-digit UTC microseconds; write-at-once; diretórios ordenados; arquivos extremamente longos/Unicode preservados nos metadados; duplicidades, renome e checksum divergentes detectados.

**G2** — `new/sql`, `validate`, `plan`, `up`, `down`, `status`, `history` sobre SQLite; Up/Down em arquivo único e pares; schema metadata; parser com cases de semicolon; readonly realmente sem writes.

**G3** — Jornal executando antes/depois de statements, sem retry cego; `doctor` apresenta evidências; concorrência/queda reproduzível; recovery fecha estado apenas quando demonstrável.

**G4 — aprovado em 2026-10-24** — Advisory lock pinned; atomicidade de
state+DDL em migração transacional; `CREATE INDEX CONCURRENTLY` explicitamente
fora de txn com journal/reconciliação de catálogo; PostgreSQL 16/17/18; TLS com
CA confiável e rejeição sem CA; PgBouncer 1.26.1-p0 em transaction pooling
hostil bloqueado; CLI `plan/up/doctor/down/history` executada em cada versão.

**G5 — aprovado em 2026-10-24** — MySQL 8.4 e MariaDB 10.11.14, 11.4.13
e 11.8.2 homologados separadamente. A jornada CLI verificou `GET_LOCK` com
nome de até 64 caracteres, concorrência, liberação explícita e no retorno da
CLI, deadlock InnoDB sem perder o lock de sessão, DDL com commit implícito,
falha no segundo e no terceiro de três steps, interrupção durante SQL, liberação
do lock após perda da sessão e reconciliação manual aprovada sem retry cego.
TLS com CA confiável e hostname divergente foi verificado em MySQL 8.4 e
MariaDB 10.11.14. Execuções locais usaram instâncias descartáveis em loopback;
isso não declara suporte a outras versões nem autoriza uso em produção.

**G6 — aprovado em 2026-10-24** — AST declarativa selada e builders validados;
Up/Down não vazios e declarados separadamente; compilação offline determinística
para SQLite, PostgreSQL, MySQL e MariaDB; fingerprints versionados de AST e SQL;
erro fail-closed para tipos/ações sem semântica suportada; escape hatch limitado a
statement único com dialect, classe de segurança e shape de retorno explícitos.
`CREATE TABLE`/`DROP TABLE`, PK, UNIQUE, CHECK, FK e índices foram compilados e
executados com validação de constraints em SQLite, PostgreSQL 16/17/18, MySQL
8.4 e MariaDB 10.11.14/11.4.13/11.8.2, usando somente instâncias descartáveis
em loopback. No fechamento de G6, a DSL ainda era compile-only; registro e
execução foram concluídos em F7/G7 abaixo.

**G7 — aprovado em 2026-10-24** — Registry reúne SQL embedded, DSL e migrations Rust
com SQL de filesystem no mesmo plano e na mesma execução/journal. CLI delega ao
runner explicitamente configurado; handshake JSON Lines valida versões,
capabilities e fingerprints do runner binário, manifest, plano e destino antes
de mutações. Runner ausente, incompatível, em timeout ou com framing/fingerprint
inválido falha sem fallback ou SQL parcial. A jornada SQLite verificou
validate/plan/status/history/doctor, Up/Down mistos e journal dos callbacks Rust;
testes cobrem fonte embedded sem arquivo implantado, identidade/checksum
determinísticos e rejeição de fingerprints divergentes. `doctor reconcile` em
modo runner continua fail-closed. Rust executável é código nativo confiável,
não sandbox; migrations Rust ainda exigem transação. Evidência local: format,
Clippy, build e `cargo test --workspace --all-features --locked` passaram em
2026-10-24: 98 testes aprovados, 17 opt-in ignorados, sem falhas. Isso aprova
G7, não conclui a V1.

**F8/G8 aprovados localmente (2026-10-25)** — `dbm import
inspect/plan/apply/verify/adopt` suporta SQLite, PostgreSQL, MySQL e MariaDB
para os formatos convencionais Knex, Goose e SQLx. `apply` exige `--yes` e o
digest exato do plano, adquire o lock nativo da instalação, relê o histórico no
mesmo alvo e registra estado corrente, timeline e proveniência sem executar SQL
legado nem alterar tabelas da ferramenta anterior; import concorrente e repetido
é idempotente. `verify` compara origem, eventos, estado corrente e digests.
`adopt` exige verificação atual, migration nativa reversível já aplicada,
evidência externa, ator, motivo e aprovação; Down permanece bloqueado enquanto
existir migration legada corrente não adotada. Metadata v4 relacional e upgrades
explícitos v1/v2/v3→v4 permanecem obrigatórios, sem upgrade automático em leitura
ou planejamento.

**Evidência G8:** testes reais e isolados em PostgreSQL 16, MySQL 8.4 e MariaDB
11.4.13. Testes de conformidade por adapter leem e persistem fixtures Knex,
Goose e SQLx, preservando filename/batch, sentinela Goose versão zero, checksum
SHA-384 e estado falho SQLx; Knex ativo bloqueia o cutover. Jornadas CLI opt-in
PostgreSQL/Goose, MySQL/Knex e MariaDB/SQLx passaram por inspect/plan, aplicação
concorrente e repetida, detecção/restauração de drift, prova de que inspect/plan
não inicializam metadata e de que import apply não executa migration nativa,
verify, bloqueio de Down antes de adoção, adoção auditada e Down depois da
adoção. As regressões SQLite também passaram. Isso aprova G8 somente para essa
evidência; não amplia a matriz de versões homologadas nem autoriza cutover em
produção.

**G8 — aprovado localmente em 2026-10-25** — provenance e histórico dos três
formatos preservados; import real não executa SQL legado; SQLx falho e lock
Knex ativo falham fechados; drift, concorrência, idempotência e bloqueio de
rollback foram verificados nos adapters de rede testados, com adoção explícita
demonstrada.

**G9** — Repeatables por checksum + audit; múltiplas fontes e namespaces; todos os comandos críticos JSON/exit code; production policy e target fingerprint.

**F9 concluída; G9 verificado localmente (2026-10-26):** metadata v5, repeatables
com estado corrente por checksum e runs auditados, exclusão do `down`, ordenação
por fonte/nome, filtro de namespace e propagação do override CLI pelo handshake
do runner. A suíte all-features passou (112 testes unitários e 24 de
integração/CLI; 24 testes opt-in ignorados). Os testes SQLite cobrem repeatables,
JSON/exit codes, política de produção e fingerprints. Novos testes reais
PostgreSQL 16.15/17.11/18.6, MySQL 8.4.11 e MariaDB 10.11.14/11.4.13/11.8.2
percorreram upgrade v3→v5 e downgrade controlado para fixture v4 seguido de
upgrade v4→v5, confirmando metadata v5 e preservação da migration aplicada e do
journal (runs/steps). Nos adapters MySQL e MariaDB, o preview permaneceu
read-only e a mutação exigiu aprovação explícita. Isso verifica G9 na matriz
suportada declarada; não amplia suporte a outras versões, não autoriza alterações
em produção e não conclui a V1.

**F10 concluída; G10 verificado localmente (2026-10-26):** feature Cargo `ai`
opcional, provider OpenAI-compatible via `reqwest` e API pública `AiProvider`
substituível. Profiles guardam endpoint/modelo/limites e apenas a referência da
variável de ambiente do segredo; AI continua desabilitada por padrão. Os comandos
explícitos `generate`, `review`, `explain` e `suggest-down` validam respostas
estritas e schemas públicos, aplicam redaction e limites de contexto, validam SQL
localmente e não escrevem arquivos, conectam ao banco ou aplicam propostas.
`--preview-context` não resolve segredo nem chama provider. Testes cobrem provider
injetado e HTTP local falso, rejeição sem opt-in, respostas/erros limitados e
redigidos, fronteiras de path/symlink, schemas, os quatro contratos CLI e ausência
de escrita/auto-apply. `cargo fmt --all -- --check`, `cargo check --workspace
--all-targets --all-features --locked`, Clippy com `-D warnings`, builds padrão,
`ai` e `all-features`, e `cargo test --workspace --all-features --locked`
passaram: 112 testes unitários e 24 de integração/CLI; doc-tests: 0. Outros 24
testes opt-in foram ignorados por exigirem bancos descartáveis. Não foram chamados providers reais;
G10 comprova os limites locais do contrato, não a qualidade/segurança de qualquer
serviço remoto.

**G10 — verificado localmente:** BYOK por referência, provider substituível,
schemas/prompt contracts, preview de contexto seguro, nenhuma chamada sem opt-in
e nenhuma transição AI→apply têm cobertura automatizada e builds padrão/AI
verificados. Isso não aprova G11 ou a V1.

**F11 — parcial (2026-10-27; G11 pendente):** o baseline local e os testes de
falha anteriores permanecem registrados. Foram adicionados dois alvos libFuzzer
(`sql_scanner` e `migration_parser`) com corpus inicial, wrappers internos sob a
feature opt-in `fuzzing-harness`, dois exemplos executáveis, `deny.toml`, script
de auditoria/SBOM e job de supply chain com ferramentas fixadas. O CI passou a
executar os exemplos e a guardar SBOM CycloneDX como artefato temporário. No
rerun manual `40439858818`, attempt 2, o job de qualidade passou (format,
Clippy, testes, build e exemplos); o job supply-chain parou em `cargo-deny` ao
rejeitar `rustls-pemfile 2.2.0` (`RUSTSEC-2025-0134`).
As versões upstream foram verificadas em 2026-10-26. No sandbox offline local
faltam `cargo-fuzz`,
`libfuzzer-sys`, `cargo-audit`, `cargo-deny`, `cargo-cyclonedx` e nightly; por
isso não há evidência local de fuzz guiado ou SBOM. O `cargo audit` remoto
completou com um warning permitido; `cargo-deny` bloqueou o job pelo crate não
mantido. Para preservar a política `unmaintained = "all"`, o código agora troca
`rustls-pemfile` pela API PEM de `rustls-pki-types`; testes e supply-chain CI desta
alteração passaram no job de qualidade e no `cargo-deny` do run `40452036445`;
o job supply-chain falhou depois porque o nome default do arquivo gerado pelo
`cargo-cyclonedx` não correspondia a `bom.json`, que o script tentava validar.
Foi adicionado `--override-filename bom`; o run `40452962185`, no commit
`980629d`, passou nos dois jobs. A auditoria de dependências, geração e validação
do SBOM, e upload do artefato passaram; o `bom.json` baixado foi parseado como
CycloneDX 1.3 com 238 componentes. Um smoke posterior de Docker rootless verificou
limites configuráveis de CPU, memória, processos e tmpfs, com filesystem
read-only e rede desativada;
nenhum código-alvo foi executado. A resolução offline do workspace parou porque
`combine v4.6.9` não está no cache local; nenhum pacote foi baixado. Smokes
limitados de promoção parent-side passaram para 16 bytes sintéticos de stdout e
dois arquivos sintéticos de 19 e 18 bytes escritos em scratch isolado, com
descritores de diretório, allowlist, limite de bytes e criação exclusiva;
arquivos gerados pelo target não foram testados. Em 2026-10-26 foi feita uma
revisão de segurança source-only do repositório no commit
`9a8dd95c3143fe9adc59f1c51fba820ad05db129`: 20 unidades foram mapeadas,
13 encerradas por revisão de fonte, quatro fingerprints únicos continuam
`needs_validation`, dois candidatos foram rejeitados e uma unidade de supply
chain ficou bloqueada. Nenhum candidato foi confirmado. A revisão não executou
código-alvo: os smokes limitados não substituem dependências offline completas,
sandbox integral do target nem validação da saída em arquivos do target. Não
equivale à auditoria completa nem a uma declaração de ausência de issues
críticas. A matriz de fault injection de
12 cenários não foi concluída; a resolução transitiva do pacote fuzz ainda não
tem lockfile próprio auditado. A cobertura existente inclui kill durante
transação SQLite, interrupção de processo MySQL não transacional e perda de
sessão PostgreSQL, com testes de banco opt-in. Documentação de recuperação e
artefatos draft de F12 foram preparados. G11 não foi aprovado.

**Atualização 2026-10-29:** `cargo test --workspace --all-features --locked
--offline` passou nos grupos unitários, CLI SQLite, DSL, importação de legado e
registry, mas dois dos cinco testes de `runner_protocol` excederam o timeout
configurado de 30 segundos. Uma repetição serial de `runner_protocol` não
terminou dentro do limite local de 420 segundos. Havia outros jobs Cargo e carga
elevada no host; isso pode ter contribuído, mas a causa não foi comprovada. O
rerun remoto da CI de `ef9b172` passou no job de qualidade completo. O resultado
local é inconclusivo, não evidência de sandbox, nem substitui fuzzing, a matriz
de 12 cenários ou as quatro validações pendentes; G11/G12 seguem pendentes.

**Fuzz — atualização 2026-10-29:** em scratch rootless foram instalados
`cargo-fuzz 0.13.2`, `cargo-audit 0.22.2`, `cargo-deny 0.20.2` e nightly
`1.101.0-nightly`; um `fuzz/Cargo.lock` de 242 pacotes foi gerado fora do
checkout. O fetch/audit transitivo não concluiu porque o `/tmp` compartilhado
(tmpfs de 7,8 GiB) ficou sem espaço ao desempacotar crates de outras plataformas.
Nenhum alvo fuzz foi executado, o lockfile não foi incorporado nem aprovado, e
G11 continua pendente. Retomar em scratch com espaço suficiente, terminar a
auditoria do grafo completo e executar cada alvo em container rootless sem rede.

**G11** — Sem duplicidade ou reaplicação indevida em cenários de kill/disconnect; fuzz scanner; docs de recuperação; performance baseline; SBOM; dependências auditadas; exemplos executáveis.

**G12** — Tag estável com release notes, checksums assinados quando possível, binários homologados, changelog, upgrade guide, compatibilidade semver, matriz de testes publicada e exemplos de CI de deploy seguros.

**Preparação F12 (2026-10-26; sem aprovação de G12):** rascunhos de changelog,
guia de upgrade de metadata, matriz de suporte e checklist estão em
`CHANGELOG.md`, `docs/upgrade-guide.md`, `docs/support-matrix.md` e
`docs/release-checklist.md`. Nenhum binário multiplataforma foi homologado,
nenhum artefato público foi publicado e nenhuma tag foi criada. G12 depende de
G11, das decisões ainda provisórias de V1 e de autorização explícita para
publicação.

---

## 18. Backlog de epics e tasks (referência para issues)

### E01 — Plataforma

- E01-01 Workspace/cargo features/edition/MSRV/license.
- E01-02 Public API policy, semver e documentação rustdoc.
- E01-03 CI builds, cargo-deny/audit, format/clippy/test.
- E01-04 CLI error taxonomy, JSON schema versioning.
- E01-05 ADR package-by-responsibility e contratos internos.

### E02 — Identidade e sources

- E02-01 Clock UTC microseconds e atomic create.
- E02-02 NativeIdentity/LegacyIdentity e fingerprint canônico.
- E02-03 SHA-256 raw e seção Up/Down.
- E02-04 Sources filesystem/embedded/registry.
- E02-05 Detector de colisão/missing/renamed/out-of-order.

### E03 — SQL e planner

- E03-01 Lexer de annotations SQL.
- E03-02 Scanner dialect aware de statements.
- E03-03 Manifest/Plan/PlanHash.
- E03-04 Lint e risk classifiers.
- E03-05 Capability resolver e SQL opaco.
- E03-06 Read-only `plan` vs shadow `test`, isolamento de target.

### E04 — Metadados e engine

- E04-01 Cinco tabelas, DDL e migração interna.
- E04-02 Estado aplicado, runs auditados e batch IDs.
- E04-03 Step journal e estados indeterminados.
- E04-04 Lock scopes/epochs/sessions.
- E04-05 Cancellation/concurrent execution semantics.
- E04-06 Doctor/reconcile and evidence requirements.

### E05 — Drivers

- E05-01 SQLite blocking worker, transaction/lock policy.
- E05-02 PostgreSQL connection pinned, session advisory locks.
- E05-03 MySQL capabilities + journal + GET_LOCK.
- E05-04 MariaDB capability differences and homologation.
- E05-05 Driver conformance kit, custom driver example.
- E05-06 Catalog introspection e diferenças normalizadas.

### E06 — DSL e Rust

- E06-01 AST de schema com tipos/constraints.
- E06-02 Dialect compilers e errors de capability.
- E06-03 DSL Up/Down e raw SQL escape hatch.
- E06-04 Rust Migration trait e MigrationContext.
- E06-05 Runner registry e protocolo versionado.
- E06-06 Embedded sources, artefact/build fingerprint.

### E07 — Legado

- E07-01 Knex importer/fixtures/cutover.
- E07-02 Goose importer/fixtures/event timeline.
- E07-03 SQLx importer/fixtures/SHA-384.
- E07-04 Import report, verify, no-runtime-reexecution.
- E07-05 Legacy adopt e rollback safety.

### E08 — Produto e IA

- E08-01 CLI comandos/status/up/down/rollback.
- E08-02 TOML, env, targets, policy.
- E08-03 Repeatables/namespaces e seeds separadas.
- E08-04 JSON Schema, shell completions, terminal UX.
- E08-05 AI providers BYOK/secret/redaction.
- E08-06 AI generate/review/explain, patch flow.
- E08-07 Agent skill and prompts.

### E09 — Qualidade, docs e distribuição

- E09-01 Fault-injection harness.
- E09-02 Migration fixtures cross DB.
- E09-03 Benchmark suite e comparação controlada.
- E09-04 Handbook: bootstrap, deploy, recovery, importing.
- E09-05 Binary distribution e security release.
- E09-06 Example embedded, runner, custom driver.

---

## 19. Release engineering e versões

### 19.1 Versionamento

- Engine e CLI semver próprios, com versionamento coordenado na V1 para facilitar suporte.
- `metadata_format_version` separado da versão do binário; migração interna sempre explícita e reversível quando possível.
- `runner_protocol_version` separado; CLI e runner rejeitam incompatibilidade de major.
- JSON schema versionado independentemente; compatibilidade documentada.
- Pacotes de drivers podem evoluir em minor releases contanto que mantenham o contrato público.

### 19.2 Entrega

- GitHub Releases Linux x86_64/aarch64, macOS ARM64/x86_64 e Windows x64 somente após CI E2E.
- `cargo install` para CLI e runner templates, sem requisito de registry privado.
- Publicação no crates.io para core/engine/drivers/lib/CLI conforme política de nomes.
- Features de build minimal e full; documentação de TLS/provider DB, static linking e SQLite bundled.
- Artefatos publicados com checksums, licenças, changelog e SBOM. Signed provenance/sigstore onde infraestrutura permitir.
- Release candidate alimentado por tests matrix noturna e fault-injection antes da tag estável.

### 19.3 Compatibilidade de banco e governança

- Matriz de banco/server version em página versionada no repositório.
- Linha que falhar na suíte de conformidade perde selo de homologação; nunca ocultar sob "compatível com MySQL".
- API de driver de terceiros recebe guia de breaking changes, exemplos, trait docs e conformance kit.
- Política para CVEs, divulgação responsável, security advisories e suporte de branches de release.

---

## 20. V2 e V3 — evolução planejada, sem bloquear V1

### V2: introspecção avançada e produtividade

- Schema diff abrangente, normalização de catalog e snapshots com tipos/constraints/indexes/views.
- Planos declarativos com sugestões de migrations a partir de diff e análise expand-contract/zero downtime.
- MCP server local via stdio, read-only por padrão; authz/gates para ferramentas mutáveis.
- Melhor análise de plano Rust estático, test harness/fixtures declarativas.
- Dependências entre namespaces com grafo e verificação de ciclos.
- Mais drivers oficiais ou comunitários (DuckDB, CockroachDB, libSQL, etc.) somente após contrato de conformidade.
- API de eventos/telemetria mais rica e integração CI dedicada.

### V3: orquestração avançada (opt-in)

- Fluxos multi-target/multi-tenant com aprovação por alvo, checkpoints e isolamento auditado.
- Schema drift monitoring sob demanda/recorrente fora da CLI principal, sem exigir sidecar.
- Rollouts progressivos, políticas organizacionais e integração com plataformas de deploy.
- Catálogo opcional de adapters e extensions; eventual UI local/web **não obrigatória**.
- Assistência de IA com estratégias de rollout, impactos e verificações mais sofisticadas, permanecendo opt-in.

---

## 21. Riscos, decisões trade-off e mitigação

| Risco | Impacto | Mitigação / evidência necessária |
|---|---|---|
| Parser SQL insuficiente para routines/triggers | Alto | blocos explícitos, scanner dialect aware, fuzz e fixtures de produção |
| MySQL/MariaDB executarem DDL parcialmente | Crítico | journal antes/depois, lock pinned, doctor e bloqueio indeterminado |
| Perder lock de sessão no meio da migration | Crítico | detecção de sessão, fail-closed, nunca continuar por outra conexão |
| Importar histórico incompleto como verificado | Crítico | verification_status e provenance sem inventar dados |
| Fingerprint Rust não comprovar código executado | Alto | manifest+artifact fingerprint, documentação do limite e trust boundary |
| Prometer DSL universal sem compatibilidade real | Alto | capability matrix + compilation error explícito |
| Misturar SQL/DSL/Rust em runners diferentes | Crítico | runner executa plano inteiro; handshake e hash |
| `plan` tocar produção acidentalmente | Crítico | leitura somente, sem bootstrap implícito, testes negativos de side effects |
| Processo de IA vazando schema/segredos | Alto | preview, minimização, redaction, opt-in e schemas de resposta |
| Public API instável para drivers externos | Alto | trait conformance suite + semver e ADRs |
| V1 excessivamente longa para manutenção solo | Alto | fatias verticais, gates explícitos, release candidates sem chamar V1 completa |
| Problemas de compatibilidade do SQL no pooler | Alto | conexão direta/session pinning; recusar transaction pooler não compatível |

---

## 22. Definition of Done da V1 — checklist executável

- [ ] SQL único e `.up/.down.sql` funcionando nas versões homologadas.
- [ ] Rust custom e DSL tipada executáveis no runner compilado e biblioteca embedded.
- [ ] CLI SQL-only independente, sem dependência em sidecar/ORM/LLM.
- [ ] Drivers oficiais `rusqlite`, `tokio-postgres` e `mysql_async`; MySQL e MariaDB **ambos** homologados e publicados na matriz.
- [ ] Trait API e conformidade para drivers terceiros demonstradas em crate de exemplo.
- [ ] Timestamps UTC 20 dígitos; colisão, renome, missing e checksum tratados corretamente.
- [ ] Nomes completos preservados, inclusive importados e longos; nenhum truncamento silencioso.
- [ ] Cinco tabelas internas, metadados versionados, runs e steps auditáveis.
- [ ] Locking com conexão pinned e recuperação segura de falhas parciais.
- [ ] Importação de Knex, Goose e SQLx real, sem execução legada, com import verify e cutover documentado.
- [ ] Rollback legado bloqueado até conversão testada e aprovada.
- [ ] Batches, rollback seletivo, repeatables, namespaces e sources embedded funcionando.
- [ ] Seeders segregados do histórico de schema, com default de bloqueio em produção.
- [ ] `test` em shadow target isolado, distinto de `plan` read-only, funcionando nos drivers homologados.
- [ ] `plan`, `validate`, `lint`, `status`, `history`, `doctor` e JSON machine-readable estáveis.
- [ ] AI BYOK gera e revisa propostas sem aplicar diretamente e sem vazar secrets por padrão.
- [ ] Fixture de queda de processo, perda de sessão, concorrência, commit desconhecido e DDL parcial aprovado.
- [ ] `plan` não altera o banco nem metadados em hipótese alguma.
- [ ] Testes de produção segura, riscos destrutivos e comandos não interativos aprovados.
- [ ] Docs completas, exemplos compiláveis, recuperação e migração de outras ferramentas.
- [ ] Performance baseline, releases reproduzíveis, SBOM e licenciamento verificado.
- [ ] Nenhum bug aberto classificado como corrupção de histórico, dupla execução ou perda de lock.

**Bloqueadores para release:** qualquer teste em vermelho de fail-closed, semântica de locks, integridade de histórico, importação legada, invariantes da CLI em produção ou perda de credenciais.

---

## 23. ADRs que devem ser escritos antes de cada implementação

- ADR-001: Source-of-truth de identidade, timestamps UTC e hashing.
- ADR-002: Esquema metadata v1, versionamento e migrações internas.
- ADR-003: Locking por banco, escopo e perda de sessão.
- ADR-004: Transaction policy e journal para DDL não transacional.
- ADR-005: Rust migration / DSL / runner protocol / mixed plans.
- ADR-006: SQL syntax/annotations e parser por dialeto.
- ADR-007: Driver API e capability detection.
- ADR-008: Legacy import, rollback adoption e baseline.
- ADR-009: Repeatable state e batch rollback.
- ADR-010: Approval e produção, AI boundary e context policy.
- ADR-011: Logging/privacy, JSON schemas, error codes.
- ADR-012: Distribution, MSRV e SemVer.

### Template ADR

```text
Título / Status / Data
Contexto e problema
Alternativas (incluindo não fazer)
Decisão
Invariantes
Riscos e contrapartidas
Contrato público afetado
Casos de teste / evidências
Como reverter a decisão
```

---

## 24. Bibliografia técnica verificada e referências de implementação

Documentações consultadas em 21/10/2026 para ancorar comportamentos que variam entre ferramentas e bancos. A matriz concreta de versões/clientes deverá ser congelada novamente no início da implementação, pois crates e servidores continuam evoluindo.

1. Knex, migrations/rollback/locking: https://github.com/knex/knex/blob/master/docs/src/guide/migrations.md
2. Goose, SQL annotations, embedded e migrações Go: https://github.com/pressly/goose
3. SQLx `Migration` e checksum (docs.rs): https://docs.rs/sqlx/latest/sqlx/migrate/struct.Migration.html
4. SQLx core `Sha384` implementação: https://docs.rs/crate/sqlx-core/latest/source/src/migrate/migration.rs
5. PostgreSQL, advisory locks: https://www.postgresql.org/docs/current/explicit-locking.html
6. SQLite, BEGIN IMMEDIATE: https://www.sqlite.org/lang_transaction.html
7. `rusqlite`, `TransactionBehavior`: https://docs.rs/rusqlite/latest/rusqlite/enum.TransactionBehavior.html
8. `tokio-postgres` client e connection: https://docs.rs/tokio-postgres/latest/tokio_postgres/
9. `mysql_async` pool e transações: https://docs.rs/mysql_async/latest/mysql_async/
10. MySQL, GET_LOCK e limite de comprimento: https://dev.mysql.com/doc/refman/8.4/en/locking-functions.html
11. MySQL, atomic DDL não é transactional DDL: https://dev.mysql.com/doc/refman/8.4/en/atomic-ddl.html
12. MariaDB, DDL e commits implícitos: https://mariadb.com/docs/server/reference/sql-statements/transactions/sql-statements-that-cause-an-implicit-commit
13. MariaDB, GET_LOCK: https://mariadb.com/docs/server/reference/sql-functions/secondary-functions/miscellaneous-functions/get_lock

---

## 25. Instruções para implementação assistida por agentes

Para cada fase F0–F12:

1. Ler este plano, ADRs aprovadas, código existente e documentação oficial atual do driver/servidor em questão.
2. Identificar invariantes e escrever testes de falha antes da lógica mutável principal.
3. Implementar uma **jornada vertical completa** ao invés de estruturar todos os crates vazios.
4. Executar fmt/clippy/test/integração do driver afetado, documentando resultado real.
5. Atualizar fixtures de conformidade, documentação, exemplos e matriz de suporte.
6. Publicar resumo de alterações, APIs novas, riscos, breaking changes, comandos utilizados e limitações restantes.
7. Nunca mascarar tests falhos, nunca forçar checksum, nem executar DDL real em produção para demonstrar protótipo.
8. Entregar funcionalidades completas por gate; não chamar mocked backend de implementado se o contrato do gate exigir banco real.

**Primeiro incremento executável recomendado:** SQL-only + SQLite com CLI `new/validate/plan/up/down/status`, identidade estável e metadados mínimos, desde o início com uma interface `DatabaseAdapter` adequada. Em seguida, adicionar falhas/locks/journal antes de expandir para MySQL/MariaDB. A primeira iteração deve produzir uma aplicação funcional e testada, não apenas skeletons.

**Resultado esperado:** uma ferramenta de migrations para Rust com simplicidade de uso comparável a Goose, ergonomia e batches inspirados no Knex, segurança de produção e integração com a era dos agentes, mantendo o núcleo independente de IA e de qualquer servidor auxiliar.
