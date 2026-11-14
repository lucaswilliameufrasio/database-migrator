# ADR-026: Workspace do dbm e produto de cliente separado

- **Status:** Aceita em 2026-11-14. A implementação ainda não começou.
- **Data:** 2026-11-14

## Contexto

O release 1.0.0 congela a API pública, a metadata 5 e o binário `dbm` no
crate único `database-migrator`. Esse crate sempre compila CLI, runner,
importação legada, DSL, aprovação de produção, Tokio, `rusqlite` com SQLite
embutido, `tokio-postgres` e `mysql_async`. Uma feature `sqlite` não remove
esse restante.

O produto de cliente precisa de uma biblioteca SQLite pequena, síncrona, para
o core do Jirau e para um plugin Flutter com binário pronto. A API congelada
é async e a sessão SQLite usa `spawn_blocking` e `tokio::sync`, como registra
a ADR-014.

O acompanhamento vivo dos itens está em `docs/progresso-pos-1.0.md`.

## Decisão

- O corte compatível fica neste repositório, como workspace. A facade
  `database-migrator` reexporta os caminhos públicos da 1.0. As crates internas
  de driver nascem com `publish = false` e, no primeiro corte, a facade depende
  das três sempre.
- Features de engine, CLI, runner, importação legada e DSL entram depois,
  ligadas por padrão. O contrato da 1.0 é o produto completo. O embed que
  desliga features é opt-in e não retira o Tokio da facade.
- A biblioteca de cliente nasce em outro repositório, sem depender deste
  crate. Copia as invariantes do journal, não a metadata 5 nem a aprovação de
  produção. O `dbm` segue como ferramenta de autoria.
- Ampliar macOS e Windows é um job de CI no GitHub. A matriz de suporte só
  descreve o que esse job cobriu. A evidência Linux x86_64 da 1.0 não é
  reescrita.

## Consequências

Quem depende da API com as features padrão não muda código. Quem passar
`default-features = false` depois das features deixará de receber o produto
inteiro que hoje recebe. O produto de cliente não espera o workspace e não
importa as crates internas. crates.io, asset de release e migration de
produção continuam sem autorização.
