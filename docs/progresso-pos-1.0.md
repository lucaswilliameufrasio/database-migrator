# Progresso pós-1.0

> **Estado:** decisões aceitas em 2026-11-14. Nenhum item abaixo foi implementado.
> **Base:** tag `v1.0.0`, commit `9972f56adb1f01646096cb017c2c0657e8726e2c`.
> **Decisão:** [ADR-026](adr/ADR-026-workspace-e-produto-cliente.md).
> **Como atualizar:** no commit que entrega um item, mude o estado dessa linha e a data desta revisão. Não reescreva o histórico da V1 no plano mestre.

**Revisão:** 2026-11-14. Próximo passo de código: A1.

O release 1.0.0 continua valendo como foi evidenciado: Linux x86_64, metadata 5, API pública congelada, crate não publicado. Este arquivo acompanha o que vem depois. G11 e G12 do plano mestre são o critério histórico da V1.

## Decisões que este acompanhamento obedece

- O repositório `database-migrator` vira um workspace. O pacote publicado na API continua `database-migrator`. Os caminhos públicos da 1.0, o binário `dbm` e a metadata 5 permanecem.
- O primeiro corte só move os três drivers para crates internas com `publish = false`. A facade depende das três sempre. Ainda não existe feature de engine.
- As features `sqlite`, `postgres` e `mysql` vêm depois, ligadas por padrão. CLI, runner, importação legada e DSL também passam a ser features ligadas por padrão num corte seguinte.
- `default-features = false` hoje ainda compila o produto inteiro, porque `default = []` e nada está atrás de feature. Quando as features existirem, o embed enxuto será opt-in. O contrato da 1.0 é o produto completo, com as features padrão.
- A facade continua async e continua puxando Tokio. A ADR-014 deixa a conexão SQLite num worker dedicado; a sessão pública usa esse runtime. Uma biblioteca síncrona sem Tokio é outro produto.
- O produto de cliente fica num repositório novo, ainda não criado. Ele não depende do crate `database-migrator`. Leva as invariantes do journal, não o formato 5 nem a aprovação de produção. Expo e React Native ficam de fora. Flutter desktop e Flutter web esperam binário e runtime declarados.
- O `dbm` continua sendo a ferramenta de autoria. O SQL de telefone usa outro diretório e o dialeto SQLite.
- Ampliar plataforma é CI no GitHub, em runners macOS e Windows. A matriz de suporte só passa a citar o que um job verde cobriu. A matriz de falha com PostgreSQL, MySQL e MariaDB nesses sistemas é outra evidência.
- Publicação no crates.io, asset de release no GitHub e migration de produção continuam fora até autorização explícita.

Sync offline e rotação de chave de SQLCipher não entram neste acompanhamento.

## Itens

Estado usado nas tabelas: `pendente`, `em andamento`, `feito`.

### A. Workspace no repositório atual

| ID | Item | Estado |
| --- | --- | --- |
| A1 | Extrair SQLite, PostgreSQL e MySQL/MariaDB para crates internas com `publish = false`. A facade depende das três sempre. API, metadata 5, binário e comportamento iguais. Suíte atual verde em Linux. | pendente |
| A2 | Features `sqlite`, `postgres` e `mysql` ligadas por padrão. Provar que `default-features = false` com `sqlite` não compila `tokio-postgres` nem `mysql_async`. | pendente |
| A3 | CLI, runner, importação legada e DSL viram features ligadas por padrão. O binário `dbm` continua completo. | pendente |
| A4 | Documentar o embed opt-in e a diferença para o contrato da 1.0, inclusive o Tokio que permanece na facade. | pendente |

### B. Plataformas no GitHub

| ID | Item | Estado |
| --- | --- | --- |
| B1 | Jobs em runner macOS e Windows do GitHub para formatar, lint, testar, buildar e rodar os exemplos SQLite. Sem serviço de banco nesse job. | pendente |
| B2 | Atualizar a matriz de suporte só com o que o job verde cobriu. O texto da 1.0 sobre Linux x86_64 permanece como evidência daquele release. | pendente |
| B3 | Matriz de falha com PostgreSQL, MySQL e MariaDB em macOS ou Windows, somente se formos declarar suporte de servidor nesses sistemas. | pendente |

### C. Produto de cliente, repositório novo

| ID | Item | Estado |
| --- | --- | --- |
| C1 | Criar o repositório. Biblioteca SQLite síncrona, sem dependência de `database-migrator`. | pendente |
| C2 | Na abertura, antes de escrever: journal, checksum, geração do arquivo e execução desconhecida. Falha preserva o arquivo. | pendente |
| C3 | Doctor curto, sem linha de dado do usuário, seguro para anexar a um crash report. | pendente |
| C4 | Um dono da conexão. Isolate e worker pedem a esse dono. | pendente |
| C5 | Crate consumido pelo `*-core` do Jirau. | pendente |
| C6 | Plugin Flutter com `.so` por ABI Android, XCFramework iOS e checksums. `flutter pub get` baixa o binário. Editar SQL não recompila Rust. | pendente |
| C7 | Autoria e teste de cadeia no `dbm`, a partir de cada schema já publicado, contra um SQLite descartável. | pendente |

## Fora deste acompanhamento

- Publicar no crates.io ou enviar binário para release do GitHub.
- Rodar migration de produção.
- Expo, React Native, Flutter desktop e Flutter web.
- Sync offline e SQLCipher.
- Reescrever G11, G12 ou os vereditos do relatório externo.
