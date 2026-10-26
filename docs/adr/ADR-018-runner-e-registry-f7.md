# ADR-018: Registry, runner e protocolo F7

- **Status:** Implementada; G7 aprovada em 2026-10-24
- **Data:** 2026-10-24

## Contexto

F6 deixou a DSL como AST compilável, sem registry ou execução. O engine atual
descobre migrations SQL a partir de diretórios e controla planejamento,
locks, journal, execução e reconciliação. Executar migrations Rust em um
subprocesso separado depois de aplicar SQL na CLI dividiria um único plano e
poderia deixar o banco parcialmente migrado quando o runner estivesse ausente,
incompatível ou desatualizado.

## Decisão

- Manter o engine, os adapters e a CLI no crate existente durante F7. Expor um
  `MigrationRegistry` composto em compilação, com identidade e origem lógica
  determinísticas, e compor SQL descoberto, planos DSL e migrations Rust antes
  de gerar manifest e plano. Identidades duplicadas ou versões ambíguas falham
  antes de conectar para mutação.
- Projetos SQL-only continuam podendo usar a execução direta da CLI. Projetos
  que registram migrations compiladas declaram explicitamente modo runner e o
  executável correspondente na configuração; nesse modo, **toda** operação de
  mutação é delegada antes de qualquer SQL, inclusive quando o plano contém
  SQL, DSL e Rust. Runner ausente, incompatível, timeout, erro de protocolo ou
  divergência de hash abortam a operação sem fallback para a CLI.
- O runner é uma biblioteca/API chamada explicitamente pelo binário do projeto.
  Compilar código Rust continua sendo responsabilidade explícita do usuário; a
  CLI não invoca Cargo nem compila ou interpreta migrations.
- Usar protocolo JSON Lines tipado e versionado em `stdin`/`stdout`, separado
  de logs em `stderr`, com frames limitados. A primeira troca é um handshake
  somente de leitura que informa `protocol_version`, `engine_version`,
  `metadata_schema_version`, capabilities, fingerprint do manifest, hash do
  plano, fingerprint do destino e SHA-256 do binário runner. A CLI só envia
  `execute` depois de validar o handshake. O pedido de execução ecoa esses fingerprints; o runner os
  recalcula e rejeita qualquer divergência antes da primeira mutação, além das
  revalidações sob lock do engine.
- O pedido contém somente caminho de configuração, ação e fingerprints; não
  contém URL, senha, token ou outro segredo em argumentos ou frames. O processo
  runner recebe apenas as variáveis de ambiente explicitamente permitidas pela
  configuração, incluindo as referências de conexão necessárias. Diagnósticos
  de protocolo não incluem valores de variáveis.
- Executar migrations Rust é execução de código nativo confiável do projeto,
  com autoridade do usuário do processo. O runner **não é sandbox**: código
  registrado pode acessar arquivos, rede ou iniciar subprocessos por conta
  própria. A API do engine controla apenas operações feitas pelo contexto de
  migration e não promete conter efeitos externos arbitrários.
- API embedded chama o mesmo registry/engine explicitamente pela aplicação;
  importar ou inicializar a biblioteca nunca executa migrations.

## Alternativas consideradas

- **CLI aplica SQL e chama o runner apenas para Rust:** rejeitada por permitir
  execução parcial e locks/journals concorrentes para um único batch.
- **Inferir migrations Rust procurando arquivos convencionais no projeto:**
  rejeitada porque não revela o registry realmente compilado e pode omitir
  módulos ou fontes com nomes não convencionais. O modo runner é declaração
  explícita e fail-closed.
- **Protocolo em argumentos ou saída textual livre:** rejeitado por expor dados
  e tornar incompatibilidade, truncamento e parsing ambíguo.
- **Compilar o runner automaticamente pela CLI:** rejeitada porque executaria
  build scripts/dependências e código arbitrário como efeito colateral de um
  comando de migration.
- **Prometer sandbox para migrations Rust:** rejeitada; um subprocesso ou
  timeout isolado não restringe filesystem, rede nem descendentes.

## Consequências e critérios de G7

- Mudanças no registry, protocolo e manifesto exigem versionamento explícito;
  incompatibilidade é erro, nunca negociação silenciosa para capacidades
  menores.
- `plan`, `up` e `down` em modo runner precisam usar o manifesto completo. Uma
  falha de handshake deve ser testada com prova de que nenhum statement SQL foi
  aplicado.
- Testes devem cobrir fontes embedded sem filesystem, identidades/checksums
  estáveis, DSL compilada, migrations Rust rastreadas pelo contexto, framing
  inválido/oversized, versão incompatível e divergência de manifesto/plano/
  destino e runner fingerprint.
- G7 só será marcado aprovado após uma jornada real com SQL + DSL + Rust no
  mesmo plano e lock, além dos casos de falha de handshake e ausência de
  execução parcial. A jornada SQLite em `tests/runner_protocol.rs` e
  `tests/registry.rs` verificou o plano misto, journal, rollback, operações
  read-only sem bootstrap, runner ausente/incompatível/timeout, framing extra,
  e fingerprints divergentes antes de mutação. Gates locais de format, Clippy,
  testes e build passaram em 2026-10-24; nove testes de integração opt-in para
  bancos externos foram ignorados. Esta aprovação de G7 não conclui a V1.
