# Progresso pós-1.0

> **Estado:** C1 feito em 2026-11-15. Próximo passo: C2. B3 continua pendente.
> **Base:** tag `v1.0.0`, commit `9972f56adb1f01646096cb017c2c0657e8726e2c`.
> **Decisão:** [ADR-026](adr/ADR-026-workspace-e-produto-cliente.md).
> **Como atualizar:** no commit que entrega um item, mude o estado dessa linha e a data desta revisão. Não reescreva o histórico da V1 no plano mestre.

**Revisão:** 2026-11-15. Próximo passo: C2. B3 continua pendente.

O release 1.0.0 continua valendo como foi evidenciado: Linux x86_64, metadata 5, API pública congelada, crate não publicado. Este arquivo acompanha o que vem depois. G11 e G12 do plano mestre são o critério histórico da V1.

## Decisões que este acompanhamento obedece

- O repositório `database-migrator` vira um workspace. O pacote publicado na API continua `database-migrator`. Os caminhos públicos da 1.0, o binário `dbm` e a metadata 5 permanecem.
- O primeiro corte moveu os três drivers para crates internas com `publish = false`. As features `sqlite`, `postgres`, `mysql`, `cli`, `runner`, `legacy` e `dsl` estão ligadas por padrão. A feature `mysql` também cobre MariaDB. A feature `cli` puxa `runner` e `legacy`. O binário `dbm` exige `cli`, e o build padrão continua completo porque essa feature está no default.
- `default-features = false` com `sqlite` não compila `tokio-postgres`, `mysql_async` nem `clap`. O contrato da 1.0 continua o produto completo, com as features padrão. Tokio e `rusqlite` permanecem na facade. Desligar `legacy` na facade não remove a inspeção legada do core nem dos drivers de PostgreSQL e MySQL/MariaDB.
- A facade continua async e continua puxando Tokio. A ADR-014 deixa a conexão SQLite num worker dedicado; a sessão pública usa esse runtime. Uma biblioteca síncrona sem Tokio é outro produto.
- O produto de cliente fica no repositório público [dbm-sqlite](https://github.com/lucaswilliameufrasio/dbm-sqlite). Ele não depende do crate `database-migrator`. Leva as invariantes do journal, não o formato 5 nem a aprovação de produção. Expo e React Native ficam de fora. Flutter desktop e Flutter web esperam binário e runtime declarados.
- O `dbm` continua sendo a ferramenta de autoria. O SQL de telefone usa outro diretório e o dialeto SQLite.
- Ampliar plataforma é CI no GitHub, em runners macOS e Windows. A matriz de suporte só passa a citar o que um job verde cobriu. A matriz de falha com PostgreSQL, MySQL e MariaDB nesses sistemas é outra evidência.
- Publicação no crates.io, asset de release no GitHub e migration de produção continuam fora até autorização explícita.

Sync offline e rotação de chave de SQLCipher não entram neste acompanhamento.

## Itens

Estado usado nas tabelas: `pendente`, `em andamento`, `feito`.

### A. Workspace no repositório atual

| ID | Item | Estado |
| --- | --- | --- |
| A1 | Extrair SQLite, PostgreSQL e MySQL/MariaDB para crates internas com `publish = false`. A facade depende das três sempre. API, metadata 5, binário e comportamento iguais. Suíte atual verde em Linux. | feito |
| A2 | Features `sqlite`, `postgres` e `mysql` ligadas por padrão. Provar que `default-features = false` com `sqlite` não compila `tokio-postgres` nem `mysql_async`. | feito |
| A3 | CLI, runner, importação legada e DSL viram features ligadas por padrão. O binário `dbm` continua completo. | feito |
| A4 | Documentar o embed opt-in e a diferença para o contrato da 1.0, inclusive o Tokio que permanece na facade. | feito |

A1 criou `database-migrator-core`, `database-migrator-sqlite`, `database-migrator-postgres` e `database-migrator-mysql`, todas com `publish = false` e requisito `=1.0.0`. Nesse corte a facade ainda dependia das três, sem feature de engine. Metadata 5 e o binário `dbm` permanecem.

O módulo público `config` passou a alcançar, com `#[doc(hidden)]`, tipos e campos que na 1.0 eram `pub(crate)`. Nenhum caminho público foi removido. A superfície não é idêntica byte a byte. O teste ignorado de upgrade da metadata MySQL ficou na facade, porque chama o engine. Três auxiliares desse teste estão `pub` com `#[doc(hidden)]` na crate MySQL.

No Linux, em 2026-11-14, passaram formatação, `git diff --check`, Clippy com `-D warnings`, `cargo test --workspace --all-features --locked` (162 passed, 49 ignored), o build, os dois exemplos SQLite e `scripts/supply-chain.sh` nos dois lockfiles. A matriz de falha com servidor não rodou.

A2, em 2026-11-14, ligou `sqlite`, `postgres` e `mysql` por padrão. A feature `mysql` cobre MariaDB. Com uma dessas features desligada, o adapter correspondente responde que ela é exigida. O build padrão continua instalando os três.

`cargo tree -p database-migrator --no-default-features --features sqlite`, nas arestas normal, dev e build, não contém `tokio-postgres`, `mysql_async`, `database-migrator-postgres` nem `database-migrator-mysql`. O mesmo perfil passou no Clippy da lib, do binário e dos exemplos. `rusqlite` e Tokio continuam na facade. `tokio-postgres` e `mysql_async` são dependências diretas opcionais das features de servidor, as mesmas crates que os drivers já usavam, para os testes de servidor compilarem junto com a feature e sumirem do perfil SQLite. A suíte `--all-features` repetiu 162 passed e 49 ignored. A matriz de falha com servidor não rodou.

A3, em 2026-11-14, ligou `cli`, `runner`, `legacy` e `dsl` por padrão, junto com as três engines. A feature `cli` puxa `clap`, `database-migrator-core/cli`, `runner` e `legacy`. O binário `dbm` declara `required-features = ["cli"]`. O `cargo build` padrão continua gerando o binário completo, porque `cli` está no default. Sem `cli`, o Cargo omite o binário. `required-features` não liga a feature. `ai`, o fixture de teste do runner e o harness de fuzz continuam fora do default. O exemplo `sqlite_schema_dsl` exige `sqlite` e `dsl`. O exemplo embutido exige `sqlite`.

No mesmo perfil SQLite, o `cargo tree` também não contém `clap`. O grafo padrão ainda contém `clap`. O Clippy desse perfil, com `--bins --examples`, passou sem warning. O binário `dbm` e o exemplo DSL ficam de fora, porque `cli` e `dsl` estão desligadas. SQL e Rust no registry permanecem. `validate_database_info` continua no engine e devolve sucesso quando `dsl` está desligada. O modelo de inspeção legada continua na crate core. PostgreSQL e MySQL/MariaDB ainda compilam `inspect_legacy_history` quando a feature da engine está ligada. Desligar `legacy` na facade não remove esses métodos. `rusqlite` e Tokio continuam na facade.

A suíte `--all-features` repetiu 162 passed e 49 ignored. Formatação, `git diff --check`, Clippy do workspace com `-D warnings`, o build, os dois exemplos SQLite e `scripts/supply-chain.sh` passaram. Os dois lockfiles não mudaram. A matriz de falha com servidor não rodou. O CI remoto não rodou.

A4, em 2026-11-14, documentou esse embed em `docs/embed.md` e apontou o texto no README. O contrato da 1.0 continua o produto com as features padrão. O perfil `default-features = false` com `sqlite` continua sem `clap` e sem os drivers de servidor, e continua com Tokio e `rusqlite` na facade. A biblioteca de cliente continua sendo outro repositório, síncrona, sem este crate. Este corte não alterou código nem lockfile. A lista direta de dependências desse perfil foi relida. O gate de compilação não foi repetido.

### B. Plataformas no GitHub

| ID | Item | Estado |
| --- | --- | --- |
| B1 | Jobs em runner macOS e Windows do GitHub para formatar, lint, testar, buildar e rodar os exemplos SQLite. Sem serviço de banco nesse job. | feito |
| B2 | Atualizar a matriz de suporte só com o que o job verde cobriu. O texto da 1.0 sobre Linux x86_64 permanece como evidência daquele release. | feito |
| B3 | Matriz de falha com PostgreSQL, MySQL e MariaDB em macOS ou Windows, somente se formos declarar suporte de servidor nesses sistemas. | pendente |

B1, em 2026-11-14, adicionou o job `quality-platforms` em `.github/workflows/ci.yml`. A matriz usa `macos-latest` e `windows-latest`, com `fail-fast: false`. Cada runner formata, roda Clippy com `-D warnings`, `cargo test --workspace --all-features --locked`, o build e os dois exemplos SQLite. Não há `services` nem URL de banco, então os testes de servidor continuam ignorados. Os jobs Linux de qualidade, supply-chain e performance permanecem. O checkout continua `actions/checkout` v7.0.1 (`3d3c42e5aac5ba805825da76410c181273ba90b1`), conferido na release upstream em 2026-11-14. A matriz de suporte não mudou. O CI remoto não rodou.

B2, em 2026-11-15, registrou o job verde. O push `43743366222` no commit `96ff75eff5e698719eb3abefb0158ba2287cfdd9` passou em `macos-latest` (imagem `macos-26-arm64`) e `windows-latest` (imagem `windows-2025`), além dos jobs Linux. macOS: 162 passed e 49 ignored. Windows: 152 passed e 49 ignored; dez testes só de Unix não entram na compilação. Não havia serviço de banco. PostgreSQL, MySQL e MariaDB continuam sem evidência nesses sistemas. O texto da 1.0 sobre Linux x86_64 permanece. B3 continua pendente e só entra se formos declarar suporte de servidor em macOS ou Windows.

### C. Produto de cliente, repositório novo

| ID | Item | Estado |
| --- | --- | --- |
| C1 | Criar o repositório. Biblioteca SQLite síncrona, sem dependência de `database-migrator`. | feito |
| C2 | Na abertura, antes de escrever: journal, checksum, geração do arquivo e execução desconhecida. Falha preserva o arquivo. | pendente |
| C3 | Doctor curto, sem linha de dado do usuário, seguro para anexar a um crash report. | pendente |
| C4 | Um dono da conexão. Isolate e worker pedem a esse dono. | pendente |
| C5 | Crate consumido pelo `*-core` do Jirau. | pendente |
| C6 | Plugin Flutter com `.so` por ABI Android, XCFramework iOS e checksums. `flutter pub get` baixa o binário. Editar SQL não recompila Rust. | pendente |
| C7 | Autoria e teste de cadeia no `dbm`, a partir de cada schema já publicado, contra um SQLite descartável. | pendente |

C1, em 2026-11-15, criou o repositório público [dbm-sqlite](https://github.com/lucaswilliameufrasio/dbm-sqlite), commit `f5deb4297c1eb8dd6ba4ab8aa0f9ce8139550598`. A crate `dbm-sqlite` 0.1.0 abre um arquivo SQLite na thread que chama e devolve a conexão. O SQLite vem embutido no `rusqlite` 0.41.1. `publish = false`. Edition 2024, Rust 1.89, toolchain do repositório 1.99.1. A consulta ao crates.io em 2026-11-15 respondeu que o crate `dbm-sqlite` não existe; nada foi publicado. O `cargo tree` nas arestas normal, dev e build não contém `tokio` nem `database-migrator`. No Linux local passaram formatação, `git diff --check`, Clippy com `-D warnings`, `cargo test --workspace --all-features --locked` (4 passed, 0 ignored, e 1 doctest) e o build. O push `43748371491` passou o job de qualidade em `ubuntu-latest`: 4 passed, 0 ignored, e 1 doctest. O journal continua no C2. B3 continua pendente.

## Fora deste acompanhamento

- Publicar no crates.io ou enviar binário para release do GitHub.
- Rodar migration de produção.
- Expo, React Native, Flutter desktop e Flutter web.
- Sync offline e SQLCipher.
- Reescrever G11, G12 ou os vereditos do relatório externo.
